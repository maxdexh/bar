use std::{ffi::OsString, sync::Arc};

use futures::Stream;
use serde::{Deserialize, Serialize};
use system_tray::item::StatusNotifierItem;
use tokio::{
    sync::{broadcast, mpsc},
    task::JoinSet,
};
use tokio_stream::StreamExt as _;

use crate::{
    clients::{
        pulse::{PulseDeviceKind, PulseDeviceState, PulseState},
        upower::{BatteryState, EnergyState},
    },
    data::{BasicDesktopState, WorkspaceId},
    display_panel::{PanelEvent, PanelInteract, PanelUpdate},
    tui,
};

pub async fn controller_spawn_panel(
    _: &std::path::Path,
    display: &str,
    envs: Vec<(OsString, OsString)>,
    _: &tokio::sync::mpsc::UnboundedSender<impl Sized>,
) -> anyhow::Result<tokio::process::Child> {
    let child = tokio::process::Command::new("kitten")
        .envs(envs)
        .stdout(std::io::stderr())
        .args([
            "panel",
            &format!("--output-name={display}"),
            // Allow logging to $KITTY_STDIO_FORWARDED
            "-o=forward_stdio=yes",
            // Do not use the system's kitty.conf
            "--config=NONE",
            // Basic look of the bar
            "-o=foreground=white",
            "-o=background=black",
            // location of the bar
            &format!("--edge={}", super::EDGE),
            // disable hiding the mouse
            "-o=mouse_hide_wait=0",
        ])
        .arg(&std::env::current_exe()?)
        .args(["internal", super::INTERNAL_BAR_PANEL_ARG])
        .kill_on_drop(true)
        .spawn()?;

    Ok(child)
}

pub fn control_panels(
    tasks: &mut JoinSet<()>,
    panel_upd_tx: broadcast::Sender<Arc<PanelUpdate>>,
    mut panel_ev_rx: mpsc::UnboundedReceiver<PanelEvent>,
) -> (
    impl Fn(BarUpdate) + Send + 'static + Clone + use<>,
    impl Stream<Item = BarEvent> + use<>,
) {
    let (bar_upd_tx, mut bar_upd_rx) = mpsc::unbounded_channel();
    tasks.spawn(async move {
        let mut state = BarState::default();

        while let Some(upd) = bar_upd_rx.recv().await {
            state.apply_update(upd);
            while let Ok(upd) = bar_upd_rx.try_recv() {
                state.apply_update(upd);
            }
            if panel_upd_tx
                .send(Arc::new(PanelUpdate::Display(to_tui(&state))))
                .is_err()
            {
                log::warn!("No panels to update")
            }
        }
    });
    (
        move |upd: BarUpdate| {
            bar_upd_tx.send(upd).unwrap();
        },
        futures::stream::poll_fn(move |cx| panel_ev_rx.poll_recv(cx)).map(|panel_event| {
            match panel_event {
                PanelEvent::Interact(PanelInteract {
                    location,
                    target,
                    kind,
                }) => BarEvent::Interact(Interact {
                    location,
                    kind,
                    target: match target {
                        Some(tag) => BarInteractTarget::deserialize_tag(&tag),
                        None => BarInteractTarget::None,
                    },
                }),
            }
        }),
    )
}

type Interact = crate::data::InteractGeneric<BarInteractTarget>;

#[derive(Serialize, Deserialize, Debug)]
pub enum BarEvent {
    Interact(Interact),
}

#[derive(Serialize, Deserialize, Debug)]
pub enum BarUpdate {
    SysTray(Arc<[(Arc<str>, StatusNotifierItem)]>),
    Desktop(BasicDesktopState),
    Energy(EnergyState),
    Pulse(PulseState),
    Ppd(Arc<str>),
    Time(String),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum BarInteractTarget {
    None,
    HyprWorkspace(WorkspaceId),
    Time,
    Energy,
    Ppd,
    Audio(PulseDeviceKind),
    Tray(Arc<str>),
}
impl BarInteractTarget {
    fn serialize_tag(&self) -> tui::InteractTag {
        tui::InteractTag::from_bytes(&postcard::to_stdvec(self).unwrap())
    }
    fn deserialize_tag(tag: &tui::InteractTag) -> Self {
        postcard::from_bytes(tag.as_bytes()).unwrap()
    }
}

// FIXME: Modularize, using direct access to clients
#[derive(Debug, Default, Clone)]
pub struct BarState {
    systray: Arc<[(Arc<str>, StatusNotifierItem)]>,
    desktop: BasicDesktopState,
    ppd_profile: Arc<str>,
    energy: EnergyState,
    pulse: PulseState,
    time: String,
}
impl BarState {
    fn apply_update(&mut self, update: BarUpdate) {
        match update {
            BarUpdate::SysTray(systray) => self.systray = systray,
            BarUpdate::Desktop(hypr) => self.desktop = hypr,
            BarUpdate::Energy(energy) => self.energy = energy,
            BarUpdate::Ppd(profile) => self.ppd_profile = profile,
            BarUpdate::Pulse(pulse) => self.pulse = pulse,
            BarUpdate::Time(time) => self.time = time,
        }
    }
}

fn to_tui(state: &BarState) -> tui::Tui {
    let mut subdiv = Vec::new();

    subdiv.push(tui::SubPart::spacing(tui::Constraint::Length(1)));

    for ws in state.desktop.workspaces.iter() {
        // FIXME: Green active_ws
        subdiv.extend([
            tui::SubPart {
                constr: tui::Constraint::Length(ws.name.chars().count() as _),
                elem: tui::Element {
                    tag: Some(BarInteractTarget::HyprWorkspace(ws.id.clone()).serialize_tag()),
                    kind: tui::ElementKind::PlainText(ws.name.clone()),
                },
            },
            tui::SubPart::spacing(tui::Constraint::Length(1)),
        ])
    }

    subdiv.push(tui::SubPart::spacing(tui::Constraint::Fill(1)));

    const SPACING: u16 = 3;

    for (addr, item) in state.systray.iter() {
        for system_tray::item::IconPixmap {
            width,
            height,
            pixels,
        } in item.icon_pixmap.as_deref().unwrap_or(&[])
        {
            let mut img = match image::RgbaImage::from_vec(
                width.cast_unsigned(),
                height.cast_unsigned(),
                pixels.clone(),
            ) {
                Some(img) => img,
                None => {
                    log::error!("Failed to load image from bytes");
                    continue;
                }
            };

            // https://users.rust-lang.org/t/argb32-color-model/92061/4
            for image::Rgba(pixel) in img.pixels_mut() {
                *pixel = u32::from_be_bytes(*pixel).rotate_left(8).to_be_bytes();
            }
            let mut png_data = Vec::new();
            if let Err(err) = img.write_with_encoder(image::codecs::png::PngEncoder::new(
                std::io::Cursor::new(&mut png_data),
            )) {
                log::error!("Error encoding image: {err}");
                continue;
            }

            subdiv.extend([
                tui::SubPart {
                    constr: tui::Constraint::Auto,
                    elem: tui::Element {
                        tag: Some(BarInteractTarget::Tray(addr.clone()).serialize_tag()),
                        kind: tui::ElementKind::Image(tui::Image {
                            data: png_data,
                            format: image::ImageFormat::Png,
                            cached: None,
                        }),
                    },
                },
                tui::SubPart::spacing(tui::Constraint::Length(1)),
            ])
        }
    }

    {
        fn fmt_audio_device<const N: usize>(
            &PulseDeviceState { muted, volume, .. }: &PulseDeviceState,
            muted_symbol: &str,
            normal_symbols: [&str; N],
        ) -> String {
            format!(
                "{}{:>3}%",
                if muted {
                    muted_symbol
                } else {
                    normal_symbols[((N as f64 * volume) as usize).clamp(0, N - 1)]
                },
                (volume * 100.0).round() as u32
            )
        }
        let sink = fmt_audio_device(&state.pulse.sink, " ", [" "]); // " ", " ", 
        // FIXME: The muted symbol is double-width, the regular symbol is not
        let source = fmt_audio_device(&state.pulse.source, " ", [" "]);

        subdiv.extend([
            tui::SubPart::spacing(tui::Constraint::Length(SPACING)),
            tui::SubPart {
                constr: tui::Constraint::Length(source.chars().count() as _),
                elem: tui::Element {
                    tag: Some(BarInteractTarget::Audio(PulseDeviceKind::Source).serialize_tag()),
                    kind: tui::ElementKind::PlainText(source.into()),
                },
            },
            tui::SubPart::spacing(tui::Constraint::Length(SPACING)),
            tui::SubPart {
                constr: tui::Constraint::Length(sink.chars().count() as _),
                elem: tui::Element {
                    tag: Some(BarInteractTarget::Audio(PulseDeviceKind::Sink).serialize_tag()),
                    kind: tui::ElementKind::PlainText(sink.into()),
                },
            },
        ]);
    }

    if state.energy.should_show {
        // TODO: Time estimate tooltip
        let percentage = state.energy.percentage.round() as i64;
        let sign = match state.energy.bstate {
            BatteryState::Discharging | BatteryState::PendingDischarge => '-',
            _ => '+',
        };
        let rate = format!("{sign}{:.1}W", state.energy.rate);
        let energy = format!("{percentage:>3}% {rate:<6}");

        let ppd_symbol = match &state.ppd_profile as &str {
            "balanced" => " ",
            "performance" => " ",
            "power-saver" => " ",
            _ => "",
        };

        subdiv.extend([
            tui::SubPart::spacing(tui::Constraint::Length(SPACING)),
            tui::SubPart {
                constr: tui::Constraint::Length(ppd_symbol.chars().count() as _),
                elem: tui::Element {
                    tag: Some(BarInteractTarget::Ppd.serialize_tag()),
                    kind: tui::ElementKind::PlainText(ppd_symbol.into()),
                },
            },
            tui::SubPart {
                constr: tui::Constraint::Length(energy.chars().count() as _),
                elem: tui::Element {
                    tag: Some(BarInteractTarget::Energy.serialize_tag()),
                    kind: tui::ElementKind::PlainText(energy.into()),
                },
            },
        ]);
    }

    if !state.time.is_empty() {
        subdiv.extend([
            tui::SubPart::spacing(tui::Constraint::Length(SPACING)),
            tui::SubPart {
                constr: tui::Constraint::Length(state.time.chars().count() as _),
                elem: tui::Element {
                    tag: Some(BarInteractTarget::Time.serialize_tag()),
                    kind: tui::ElementKind::PlainText(state.time.as_str().into()),
                },
            },
        ])
    }

    subdiv.push(tui::SubPart::spacing(tui::Constraint::Length(1)));

    tui::Tui {
        root: tui::Element {
            tag: None,
            kind: tui::ElementKind::Subdivide(tui::Subdiv {
                axis: tui::Axis::Horizontal,
                parts: subdiv.into(),
            }),
        },
    }
}

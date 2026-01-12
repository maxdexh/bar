use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use anyhow::Context;
use futures::Stream;
use serde::{Deserialize, Serialize};
use system_tray::item::StatusNotifierItem;
use tokio::task::JoinSet;
use tokio_stream::StreamExt as _;
use tokio_util::{sync::CancellationToken, time::FutureExt};

use crate::{
    data::{BasicDesktopState, WorkspaceId},
    modules::{
        pulse::{PulseDeviceKind, PulseDeviceState, PulseState},
        upower::{BatteryState, EnergyState},
    },
    monitors::MonitorEvent,
    terminals::{SpawnTerm, TermEvent, TermId, TermMgrUpdate, TermUpdate},
    tui,
    utils::{Emit, ResultExt as _, SharedEmit, UnbTx, unb_chan},
};

pub async fn run_bar_panel_manager(
    monitor_rx: impl Stream<Item = MonitorEvent> + Send + 'static,
    bar_upd_rx: impl Stream<Item = BarUpdate> + Send + 'static,
    bar_ev_tx: impl SharedEmit<(BarEventInfo, BarEvent)>,
) {
    let mut tasks = JoinSet::<()>::new();

    let mut term_upd_tx;
    {
        let term_upd_rx;
        (term_upd_tx, term_upd_rx) = unb_chan();
        tasks.spawn(crate::terminals::run_term_manager(term_upd_rx));
    }

    tasks.spawn(async move {
        struct Instance {
            monitor_name: Arc<str>,
            inst_task: tokio::task::AbortHandle,
            upd_tx: UnbTx<Arc<BarState>>,
        }
        let mut instances = HashMap::<TermId, Instance>::new();
        let mut subtasks = JoinSet::<()>::new();

        enum Upd {
            Monitor(MonitorEvent),
            Broadcast(BarUpdate),
        }
        let updates = monitor_rx
            .map(Upd::Monitor)
            .merge(bar_upd_rx.map(Upd::Broadcast));
        tokio::pin!(updates);

        let mut state = BarState::default();

        fn shutdown(
            ids: impl IntoIterator<Item = TermId>,
            instances: &mut HashMap<TermId, Instance>,
            term_upd_tx: &mut impl Emit<TermMgrUpdate>,
        ) {
            for id in ids {
                if let Some(inst) = instances.remove(&id) {
                    inst.inst_task.abort();
                    term_upd_tx.emit(TermMgrUpdate::TermUpdate(id, TermUpdate::Shutdown));
                }
            }
        }
        loop {
            let upd = tokio::select! {
                Some(upd) = updates.next() => upd,
                Some(res) = subtasks.join_next() => {
                    if let Err(err) = res && !err.is_cancelled() {
                        log::error!("Error with task: {err}");
                    }
                    continue;
                }
            };

            match upd {
                Upd::Broadcast(upd) => {
                    state.apply_update(upd);
                    let state = Arc::new(state.clone());

                    let mut shutdown_queue = Vec::new();
                    for (term_id, inst) in &mut instances {
                        if inst.upd_tx.send(state.clone()).is_err() {
                            shutdown_queue.push(term_id.clone());
                        }
                    }
                    shutdown(shutdown_queue, &mut instances, &mut term_upd_tx);
                }
                Upd::Monitor(ev) => {
                    shutdown(
                        {
                            let shutdown_monitors: HashSet<_> = ev
                                .removed()
                                .chain(ev.added_or_changed().map(|it| &it.name as &str))
                                .collect();

                            instances
                                .iter()
                                .filter(|(_, inst)| {
                                    shutdown_monitors.contains(&inst.monitor_name as &str)
                                })
                                .map(|(id, _)| id.clone())
                                .collect::<Vec<_>>()
                        },
                        &mut instances,
                        &mut term_upd_tx,
                    );

                    for monitor in ev.added_or_changed() {
                        let bar_info = BarEventInfo {
                            monitor: monitor.name.clone(),
                        };
                        let term_id = TermId::from_str(&format!("BAR-{}", monitor.name));
                        let (bar_upd_tx, bar_upd_rx) = unb_chan();
                        let (term_ev_tx, term_ev_rx) = unb_chan();
                        let listener = subtasks.spawn(run_instance_controller(
                            monitor.name.clone(),
                            bar_ev_tx.clone().with(move |ev| (bar_info.clone(), ev)),
                            bar_upd_rx,
                            {
                                let term_id = term_id.clone();
                                term_upd_tx.clone().with(move |upd| {
                                    TermMgrUpdate::TermUpdate(term_id.clone(), upd)
                                })
                            },
                            term_ev_rx.map(|(_, a)| a),
                        ));
                        term_upd_tx.emit(TermMgrUpdate::SpawnPanel(SpawnTerm {
                            term_id: term_id.clone(),
                            extra_args: vec![
                                format!("--output-name={}", monitor.name).into(),
                                // Allow logging to $KITTY_STDIO_FORWARDED
                                "-o=forward_stdio=yes".into(),
                                // Do not use the system's kitty.conf
                                "--config=NONE".into(),
                                // Basic look of the bar
                                "-o=foreground=white".into(),
                                "-o=background=black".into(),
                                // location of the bar
                                format!("--edge={}", super::EDGE).into(),
                                // disable hiding the mouse
                                "-o=mouse_hide_wait=0".into(),
                            ],
                            extra_envs: Default::default(),
                            term_ev_tx,
                            cancel: CancellationToken::new(),
                        }));

                        let old = instances.insert(
                            term_id,
                            Instance {
                                monitor_name: monitor.name.clone(),
                                inst_task: listener,
                                upd_tx: bar_upd_tx,
                            },
                        );
                        assert!(old.is_none());
                    }
                }
            }
        }
    });

    if let Some(Err(err)) = tasks.join_next().await {
        log::error!("Error with task: {err}");
    }
}
async fn run_instance_controller(
    monitor_name: Arc<str>, // TODO: Use for workspaces
    mut ev_tx: impl SharedEmit<BarEvent>,
    upd_rx: impl Stream<Item = Arc<BarState>> + 'static + Send,
    mut term_upd_tx: impl SharedEmit<TermUpdate>,
    term_ev_rx: impl Stream<Item = TermEvent> + 'static + Send,
) {
    tokio::pin!(term_ev_rx);

    let Some(mut sizes) = async {
        loop {
            if let Some(TermEvent::Sizes(sizes)) = term_ev_rx.next().await {
                break sizes;
            }
        }
    }
    .timeout(Duration::from_secs(10))
    .await
    .context("Failed to receive terminal sizes")
    .ok_or_log() else {
        return;
    };

    enum Inc {
        Bar(Arc<BarState>),
        Term(TermEvent),
    }
    let incoming = upd_rx.map(Inc::Bar).merge(term_ev_rx.map(Inc::Term));
    tokio::pin!(incoming);

    let mut tui = to_tui(&BarState::default(), &monitor_name);
    let mut layout = tui::RenderedLayout::default();
    while let Some(inc) = incoming.next().await {
        let mut rerender = false;
        match inc {
            Inc::Bar(new_state) => {
                tui = to_tui(&new_state, &monitor_name);
                rerender = true;
            }
            Inc::Term(TermEvent::Crossterm(ev)) => {
                if let crossterm::event::Event::Mouse(ev) = ev
                    && let Some(tui::TuiInteract {
                        location,
                        payload: target,
                        kind,
                    }) = layout.interpret_mouse_event(ev, sizes.font_size())
                {
                    let interact = Interact {
                        location,
                        kind,
                        payload: match target {
                            Some(tag) => BarInteractTarget::from_tag(&tag),
                            None => BarInteractTarget::None,
                        },
                    };
                    ev_tx.emit(BarEvent::Interact(interact));
                }
            }
            Inc::Term(TermEvent::Sizes(new_sizes)) => {
                sizes = new_sizes;
                rerender = true;
            }
        }
        if rerender {
            let mut buf = Vec::new();
            match tui::draw_to(&mut buf, |ctx| {
                let size = sizes.cell_size;
                tui.render(
                    ctx,
                    tui::SizingContext {
                        font_size: sizes.font_size(),
                        div_w: Some(size.x),
                        div_h: Some(size.y),
                    },
                    tui::Area {
                        size,
                        pos: Default::default(),
                    },
                )
            }) {
                Err(err) => log::error!("Failed to draw: {err}"),
                Ok(new_layout) => layout = new_layout,
            }
            term_upd_tx.emit(TermUpdate::Print(buf));
            term_upd_tx.emit(TermUpdate::Flush);
        }
    }
}

type Interact = tui::InteractGeneric<BarInteractTarget>;

#[derive(Serialize, Deserialize, Debug)]
pub enum BarEvent {
    Interact(Interact),
}
#[derive(Clone, Debug)]
pub struct BarEventInfo {
    pub monitor: Arc<str>,
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
    fn into_tag(self) -> tui::InteractTag {
        Arc::new(self)
    }
    fn from_tag(tag: &tui::InteractTag) -> Self {
        let tag: &dyn std::any::Any = tag;
        tag.downcast_ref::<Self>().unwrap().clone()
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
            BarUpdate::Desktop(desktop) => self.desktop = desktop,
            BarUpdate::Energy(energy) => self.energy = energy,
            BarUpdate::Ppd(profile) => self.ppd_profile = profile,
            BarUpdate::Pulse(pulse) => self.pulse = pulse,
            BarUpdate::Time(time) => self.time = time,
        }
    }
}

fn to_tui(state: &BarState, monitor_name: &str) -> tui::Tui {
    let mut subdiv = Vec::new();

    subdiv.push(tui::StackItem::spacing(1));

    for ws in state.desktop.workspaces.iter() {
        if ws.monitor.as_deref() != Some(monitor_name) {
            continue;
        }
        subdiv.extend([
            tui::StackItem::auto(tui::InteractElem::new(
                BarInteractTarget::HyprWorkspace(ws.id.clone()).into_tag(),
                tui::Text::plain(&ws.name).styled(tui::Style {
                    fg: ws.is_active.then_some(tui::Color::Green),
                    ..Default::default()
                }),
            )),
            tui::StackItem::spacing(1),
        ])
    }

    subdiv.push(tui::StackItem::new(tui::Constr::Fill(1), tui::Elem::Empty));

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
                tui::StackItem::auto(tui::InteractElem::new(
                    BarInteractTarget::Tray(addr.clone()).into_tag(),
                    tui::Image::load_or_empty(png_data, image::ImageFormat::Png),
                )),
                tui::StackItem::spacing(1),
            ])
        }
    }

    {
        fn audio_item(
            kind: PulseDeviceKind,
            &PulseDeviceState { volume, muted, .. }: &PulseDeviceState,
            unmuted_sym: impl FnOnce() -> tui::StackItem,
            muted_sym: impl FnOnce() -> tui::StackItem,
        ) -> tui::StackItem {
            tui::StackItem::auto(tui::InteractElem::new(
                BarInteractTarget::Audio(kind).into_tag(),
                tui::Stack::horizontal([
                    if muted { muted_sym() } else { unmuted_sym() },
                    tui::StackItem::auto(tui::Text::plain(format!(
                        "{:>3}%",
                        (volume * 100.0).round() as u32
                    ))),
                ]),
            ))
        }
        subdiv.extend([
            tui::StackItem::spacing(SPACING),
            audio_item(
                PulseDeviceKind::Source,
                &state.pulse.source,
                || {
                    // There is no double-width microphone character, so we have to build or own.
                    tui::StackItem::auto(tui::Text {
                        width: 2,
                        style: Default::default(),
                        lines: [tui::TextLine {
                            height: 1,
                            // https://sw.kovidgoyal.net/kitty/text-sizing-protocol/
                            // - w=2      set width to 2
                            // - h=2      ceter the text horizontally
                            // - n=1/d=1  use fractional scale of 1:1. kitty ignores w without this
                            text: "\x1b]66;w=2:h=2:n=1:d=1;\x07".into(),
                        }]
                        .into(),
                    })
                },
                || tui::StackItem::auto(tui::Text::plain(" ")),
            ),
            tui::StackItem::spacing(SPACING),
            audio_item(
                PulseDeviceKind::Sink,
                &state.pulse.sink,
                || tui::StackItem::auto(tui::Text::plain(" ")),
                || tui::StackItem::auto(tui::Text::plain(" ")),
            ),
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
            tui::StackItem::spacing(SPACING),
            tui::StackItem::auto(tui::InteractElem::new(
                BarInteractTarget::Ppd.into_tag(),
                tui::Text::plain(ppd_symbol),
            )),
            tui::StackItem::auto(tui::InteractElem::new(
                BarInteractTarget::Energy.into_tag(),
                tui::Text::plain(energy),
            )),
        ]);
    }

    if !state.time.is_empty() {
        subdiv.extend([
            tui::StackItem::spacing(SPACING),
            tui::StackItem::auto(tui::InteractElem::new(
                BarInteractTarget::Time.into_tag(),
                tui::Text::plain(&state.time),
            )),
        ])
    }

    subdiv.push(tui::StackItem::spacing(1));

    tui::Tui {
        root: Box::new(tui::Stack::horizontal(subdiv).into()),
    }
}

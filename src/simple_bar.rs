use std::{collections::HashMap, sync::Arc};

use anyhow::Context;
use tokio::task::JoinSet;

use crate::{
    clients,
    panels::{self, BarTuiElem},
    tui,
    utils::{ReloadRx, ReloadTx, ResultExt, WatchRx, WatchTx, watch_chan},
};

struct ModuleArgs {
    tui_tx: WatchTx<BarTuiElem>,
    reload_rx: ReloadRx,
    _unused: (),
}

struct BarModuleFactory {
    reload_tx: ReloadTx,
    tasks: JoinSet<()>,
}
impl BarModuleFactory {
    fn spawn<F: Future<Output = ()> + 'static + Send>(
        &mut self,
        task: impl FnOnce(ModuleArgs) -> F,
    ) -> WatchRx<BarTuiElem> {
        let (tui_tx, tui_rx) = watch_chan(BarTuiElem::Hide);
        self.tasks.spawn(task(ModuleArgs {
            reload_rx: self.reload_tx.subscribe(),
            tui_tx,
            _unused: (),
        }));
        tui_rx
    }
    fn spawn_with<F: Future<Output = ()> + 'static + Send, C>(
        &mut self,
        ctx: C,
        task: impl FnOnce(C, ModuleArgs) -> F,
    ) -> WatchRx<BarTuiElem> {
        self.spawn(|args| task(ctx, args))
    }
    fn fixed(&mut self, elem: tui::StackItem) -> WatchRx<BarTuiElem> {
        let (tx, rx) = watch_chan(BarTuiElem::Hide);
        tx.send_replace(BarTuiElem::Shared(elem));
        rx
    }
}

pub async fn main2() {
    let mut tasks = JoinSet::new();
    let mut reload_tx = ReloadTx::new();

    let bar_tx = WatchTx::new(Vec::new());
    tasks.spawn(panels::run_manager(bar_tx.subscribe(), reload_tx.clone()));

    let mut fac = BarModuleFactory {
        reload_tx: reload_tx.clone(),
        tasks: JoinSet::new(),
    };

    let pulse = Arc::new(clients::pulse::connect(reload_tx.subscribe()));
    let modules = [
        fac.fixed(tui::StackItem::spacing(1)),
        fac.spawn(hypr_module),
        fac.fixed(tui::StackItem::new(
            tui::Constr::Fill(1),
            tui::Elem::empty(),
        )),
        fac.spawn(tray_module),
        fac.fixed(tui::StackItem::spacing(3)),
        fac.spawn_with(
            PulseModuleCtx {
                pulse: pulse.clone(),
                device_kind: clients::pulse::PulseDeviceKind::Source,
                muted_sym: tui::RawPrint::plain(" ").into(),
                unmuted_sym: tui::RawPrint::center_symbol("", 2).into(),
            },
            pulse_module,
        ),
        fac.fixed(tui::StackItem::spacing(3)),
        fac.spawn_with(
            PulseModuleCtx {
                pulse,
                device_kind: clients::pulse::PulseDeviceKind::Sink,
                muted_sym: tui::RawPrint::plain(" ").into(),
                unmuted_sym: tui::RawPrint::plain(" ").into(),
            },
            pulse_module,
        ),
        fac.fixed(tui::StackItem::spacing(3)),
        fac.spawn(ppd_module),
        fac.spawn(energy_module),
        fac.fixed(tui::StackItem::spacing(3)),
        fac.spawn(time_module),
    ];

    bar_tx.send_replace(modules.into());

    reload_tx.reload();

    if let Some(res) = tasks.join_next().await {
        res.ok_or_log();
    }
}

async fn hypr_module(
    ModuleArgs {
        tui_tx, reload_rx, ..
    }: ModuleArgs,
) {
    let hypr = Arc::new(clients::hypr::connect(reload_rx));

    let mut basic_rx = hypr.basic_rx.clone();
    basic_rx.mark_changed();

    while let Some(()) = basic_rx.changed().await.ok_or_debug() {
        let mut by_monitor = HashMap::new();
        for ws in basic_rx.borrow_and_update().workspaces.iter() {
            let Some(monitor) = ws.monitor.clone() else {
                continue;
            };
            let wss = by_monitor.entry(monitor).or_insert_with(Vec::new);

            let on_interact = tui::InteractCallback::from_fn_ctx(
                (hypr.clone(), ws.id.clone()),
                move |(hypr, ws), interact| {
                    match interact.kind {
                        tui::InteractKind::Click(tui::MouseButton::Left) => {
                            hypr.switch_workspace(ws.clone());
                        }
                        _ => {
                            // TODO: Show workspace info on rclick/hover
                        }
                    }
                    None
                },
            );

            wss.push(tui::StackItem::auto(
                tui::Elem::from(tui::RawPrint::plain(&ws.name).styled(tui::Style {
                    fg: ws.is_active.then_some(tui::Color::Green),
                    ..Default::default()
                }))
                .with_interact(on_interact),
            ));
            wss.push(tui::StackItem::spacing(1));
        }
        let by_monitor = by_monitor
            .into_iter()
            .map(|(k, v)| (k, tui::StackItem::auto(tui::Stack::horizontal(v))))
            .collect();

        tui_tx.send_replace(BarTuiElem::ByMonitor(by_monitor));
    }
}
async fn time_module(
    ModuleArgs {
        tui_tx,
        mut reload_rx,
        ..
    }: ModuleArgs,
) {
    use chrono::{Datelike, Timelike};
    use std::time::Duration;

    let tooltip = tui::HoverCallback::from_fn(move |_| {
        let now = chrono::Local::now().date_naive();
        let title = now.format("%B %Y").to_string();

        let first_day_offset = now
            .with_day(1)
            .map(|first| first.weekday() as u16)
            .context("Failed to set day")
            .ok_or_log()?;

        let num_weeks = usize::div_ceil(
            usize::from(now.num_days_in_month()) + usize::from(first_day_offset),
            7,
        );
        let mut lines = vec![vec![[tui::StackItem::spacing(2)]; 7]; num_weeks];

        for n0 in 0u16..now.num_days_in_month().into() {
            let n1 = n0 + 1;
            let day = now
                .with_day(n1.into())
                .with_context(|| format!("Failed to set day {n1}"))
                .ok_or_log()?;

            let week_in_month = (first_day_offset + n0) / 7;
            let item = &mut lines[usize::from(week_in_month)][day.weekday() as usize];
            *item = [tui::StackItem::auto({
                let it = tui::RawPrint::plain(format!("{n1:>2}"));
                if now == day {
                    it.styled(tui::Style {
                        fg: Some(tui::Color::Green),
                        ..Default::default()
                    })
                    .map_display(|styled| styled.to_string())
                } else {
                    it
                }
            })];
        }
        let weekday_line = ["Mo", "Tu", "We", "Th", "Fr", "Sa", "Su"]
            .map(|d| [tui::StackItem::auto(tui::RawPrint::plain(d))])
            .into();

        let title = tui::StackItem::auto(tui::RawPrint::plain(title).styled(tui::Style {
            modifier: tui::Modifier {
                bold: true,
                ..Default::default()
            },
            ..Default::default()
        }));
        let mut parts = vec![title];
        for line in std::iter::once(weekday_line).chain(lines) {
            let line = line.join(&tui::StackItem::spacing(1));
            let line = tui::Stack::horizontal(line);
            parts.push(tui::StackItem::auto(line));
        }
        Some(tui::Stack::vertical(parts).into())
    });

    let mut prev_minutes = 61;
    loop {
        let now = chrono::Local::now();
        let minute = now.minute();
        if prev_minutes != minute {
            let tui = tui::RawPrint::plain(now.format("%H:%M %d/%m").to_string());
            tui_tx.send_replace(BarTuiElem::Shared(tui::StackItem::auto(
                tui::Elem::from(tui).with_tooltip(&tooltip),
            )));

            prev_minutes = minute;
        } else {
            let seconds_until_minute = 60 - u64::from(now.second());
            let timeout_ms = std::cmp::max(750 * seconds_until_minute, 100);

            tokio::select! {
                Some(()) = reload_rx.wait() => {}
                () = tokio::time::sleep(Duration::from_millis(timeout_ms)) => {}
            }
        }
    }
}

struct PulseModuleCtx {
    pulse: Arc<clients::pulse::PulseClient>,
    device_kind: clients::pulse::PulseDeviceKind,
    muted_sym: tui::Elem,
    unmuted_sym: tui::Elem,
}
async fn pulse_module(
    PulseModuleCtx {
        pulse,
        device_kind,
        muted_sym,
        unmuted_sym,
    }: PulseModuleCtx,
    ModuleArgs { tui_tx, .. }: ModuleArgs,
) {
    use crate::clients::pulse::*;

    let mut state_rx = pulse.state_rx.clone();

    let on_interact = tui::InteractCallback::from_fn({
        let pulse = pulse.clone();
        move |interact| {
            pulse
                .update_tx
                .send(PulseUpdate {
                    target: device_kind,
                    kind: match interact.kind {
                        tui::InteractKind::Click(tui::MouseButton::Left) => {
                            PulseUpdateKind::ToggleMute
                        }
                        tui::InteractKind::Click(tui::MouseButton::Right) => {
                            PulseUpdateKind::ResetVolume
                        }
                        tui::InteractKind::Scroll(direction) => PulseUpdateKind::VolumeDelta(
                            2 * match direction {
                                tui::Direction::Up => 1,
                                tui::Direction::Down => -1,
                                tui::Direction::Left => -1,
                                tui::Direction::Right => 1,
                            },
                        ),
                        _ => return None,
                    },
                })
                .ok_or_log();
            None
        }
    });

    while let Some(()) = state_rx.changed().await.ok_or_debug() {
        let state = state_rx.borrow_and_update();
        let &PulseDeviceState { volume, muted, .. } = match device_kind {
            PulseDeviceKind::Sink => &state.sink,
            PulseDeviceKind::Source => &state.source,
        };
        tui_tx.send_replace(BarTuiElem::Shared(tui::StackItem::auto(
            tui::Elem::from(tui::Stack::horizontal([
                tui::StackItem::auto(if muted {
                    muted_sym.clone()
                } else {
                    unmuted_sym.clone()
                }),
                tui::StackItem::auto(tui::RawPrint::plain(format!(
                    "{:>3}%",
                    (volume * 100.0).round() as u32
                ))),
            ]))
            .with_interact(&on_interact),
        )));
    }
}
async fn energy_module(
    ModuleArgs {
        tui_tx, reload_rx, ..
    }: ModuleArgs,
) {
    use crate::clients::upower::*;
    let energy = Arc::new(clients::upower::connect(reload_rx));

    let tooltip = tui::HoverCallback::from_fn_ctx(energy.clone(), |energy, _| {
        let text = {
            let lock = energy.state_rx.borrow();
            let display_time = |time: std::time::Duration| {
                let hours = time.as_secs() / 3600;
                let mins = (time.as_secs() / 60) % 60;
                format!("{hours}h {mins}min")
            };
            match lock.battery_state {
                BatteryState::Discharging | BatteryState::PendingDischarge => {
                    format!("Battery empty in {}", display_time(lock.time_to_empty))
                }
                BatteryState::FullyCharged => "Battery full".to_owned(),
                BatteryState::Empty => "Battery empty".to_owned(),
                BatteryState::Unknown => "Battery state unknown".to_owned(),
                BatteryState::Charging | BatteryState::PendingCharge => {
                    format!("Battery full in {}", display_time(lock.time_to_full))
                }
            }
        };
        Some(tui::RawPrint::plain(text).into())
    });

    let mut state_rx = energy.state_rx.clone();
    state_rx.mark_changed();
    while let Some(()) = state_rx.changed().await.ok_or_debug() {
        let state = state_rx.borrow_and_update().clone();
        if !state.is_present {
            tui_tx.send_replace(BarTuiElem::Hide);
            continue;
        }

        // TODO: Time estimate tooltip
        let percentage = state.percentage.round() as i64;
        let sign = match state.battery_state {
            BatteryState::Discharging | BatteryState::PendingDischarge => '-',
            BatteryState::Charging | BatteryState::PendingCharge => '+',
            BatteryState::FullyCharged | BatteryState::Unknown | BatteryState::Empty => '±',
        };
        let rate = format!("{sign}{:.1}W", state.energy_rate);
        let energy = format!("{percentage:>3}% {rate:<6}");

        tui_tx.send_replace(BarTuiElem::Shared(tui::StackItem::auto(
            tui::Elem::from(tui::RawPrint::plain(energy)).with_tooltip(&tooltip),
        )));
    }
}
async fn ppd_module(
    ModuleArgs {
        tui_tx, reload_rx, ..
    }: ModuleArgs,
) {
    let ppd = Arc::new(clients::ppd::connect(reload_rx));

    let tooltip = tui::HoverCallback::from_fn_ctx(ppd.clone(), |ppd, _| {
        let profile = ppd.profile_rx.borrow().clone();
        Some(tui::PlainLines::new(profile.as_deref().unwrap_or("No profile")).into())
    });
    let on_interact = tui::InteractCallback::from_fn_ctx(ppd.clone(), |ppd, interact| {
        match interact.kind {
            tui::InteractKind::Click(tui::MouseButton::Left) => {
                ppd.cycle_profile();
            }
            _ => {
                //
            }
        }
        None
    });

    let mut profile_rx = ppd.profile_rx.clone();
    while let Some(()) = profile_rx.changed().await.ok_or_debug() {
        let Some(icon) = profile_rx
            .borrow_and_update()
            .as_deref()
            .and_then(|profile| {
                Some(match profile {
                    "balanced" => tui::RawPrint::plain(" ").into(),
                    "performance" => tui::RawPrint::center_symbol(" ", 2).into(),
                    "power-saver" => tui::RawPrint::plain(" ").into(),
                    _ => return None,
                })
            })
        else {
            tui_tx.send_replace(BarTuiElem::Hide);
            continue;
        };

        let elem: tui::Elem = icon;
        let elem = elem.with_tooltip(&tooltip).with_interact(&on_interact);

        tui_tx.send_replace(BarTuiElem::Shared(tui::StackItem::auto(elem)));
    }
}

async fn tray_module(
    ModuleArgs {
        tui_tx, reload_rx, ..
    }: ModuleArgs,
) {
    use crate::clients::tray::*;
    let tray = Arc::new(clients::tray::connect(reload_rx));

    let mk_interactivity = |addr: &Arc<str>, elem: tui::Elem| {
        let tooltip =
            tui::HoverCallback::from_fn_ctx((tray.clone(), addr.clone()), |(tray, addr), _| {
                let items = tray.state_rx.borrow().items.clone();

                // FIXME: Handle more attrs
                let system_tray::item::Tooltip {
                    icon_name: _,
                    icon_data: _,
                    title,
                    description,
                } = items
                    .get(addr)
                    .and_then(|item| item.tool_tip.as_ref())
                    .with_context(|| format!("Unknown tray addr {addr}"))
                    .ok_or_log()?;

                let tui = tui::Stack::vertical([
                    tui::StackItem::auto(tui::Stack::horizontal([
                        tui::StackItem::new(tui::Constr::Fill(1), tui::Elem::empty()),
                        tui::StackItem::auto(tui::PlainLines::new(title.as_str()).styled(
                            tui::Style {
                                modifier: tui::Modifier {
                                    bold: true,
                                    ..Default::default()
                                },
                                ..Default::default()
                            },
                        )),
                        tui::StackItem::new(tui::Constr::Fill(1), tui::Elem::empty()),
                    ])),
                    tui::StackItem::auto(tui::PlainLines::new(description.as_str())),
                ])
                .into();

                Some(tui)
            });

        let addr = addr.clone();
        let tray = tray.clone();
        let on_interact = tui::InteractCallback::from_fn(move |interact| match interact.kind {
            tui::InteractKind::Click(tui::MouseButton::Right) => {
                let TrayMenuExt {
                    menu_path,
                    submenus,
                    ..
                } = tray
                    .state_rx
                    .borrow()
                    .menus
                    .get(&addr)
                    .cloned()
                    .with_context(|| format!("Unknown tray addr {addr}"))
                    .ok_or_log()?;

                let icb = {
                    |menu_path, id| {
                        let tray = tray.clone();
                        let addr = addr.clone();
                        let menu_path = Arc::clone(menu_path);
                        tui::InteractCallback::from_fn(move |interact| {
                            tray.menu_interact_tx
                                .send(TrayMenuInteract {
                                    addr: addr.clone(),
                                    menu_path: Arc::clone(&menu_path),
                                    id,
                                    kind: interact.kind,
                                })
                                .ok_or_log();
                            None
                        })
                    }
                };

                let tui = tui::Block {
                    borders: tui::Borders::all(),
                    border_style: tui::Style {
                        fg: Some(tui::Color::DarkGrey),
                        ..Default::default()
                    },
                    border_set: tui::LineSet::thick(),
                    inner: Some(tray_menu_to_tui(
                        0,
                        &submenus,
                        menu_path
                            .as_ref()
                            .map(|menu_path| |id| icb(menu_path, id))
                            .as_ref(),
                    )),
                };
                Some(tui.into())
            }
            _ => None,
        });
        elem.with_interact(on_interact).with_tooltip(tooltip)
    };

    let mut state_rx = tray.state_rx.clone();
    while state_rx.changed().await.is_ok() {
        let items = state_rx.borrow_and_update().items.clone();
        let mut parts = Vec::new();
        for (addr, item) in items.iter() {
            // FIXME: Handle the other options
            // FIXME: Why are we showing all icons?
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

                parts.extend([
                    tui::StackItem::auto(mk_interactivity(
                        addr,
                        tui::Image {
                            img,
                            sizing: tui::ImageSizeMode::FillAxis(tui::Axis::Y, 1),
                        }
                        .into(),
                    )),
                    tui::StackItem::spacing(1),
                ])
            }
        }
        let tui = tui::Stack::horizontal(parts);
        tui_tx.send_replace(BarTuiElem::Shared(tui::StackItem::auto(tui)));
    }
    fn tray_menu_item_to_tui(
        depth: u16,
        item: &system_tray::menu::MenuItem,
        on_interact: Option<&impl Fn(i32) -> tui::InteractCallback>,
    ) -> Option<tui::Elem> {
        use system_tray::menu::*;
        let main_elem = match item {
            MenuItem { visible: false, .. } => return None,
            MenuItem {
                visible: true,
                menu_type: MenuType::Separator,
                ..
            } => tui::Block {
                borders: tui::Borders {
                    top: true,
                    ..Default::default()
                },
                border_style: tui::Style {
                    fg: Some(tui::Color::DarkGrey),
                    ..Default::default()
                },
                border_set: tui::LineSet::normal(),
                inner: None,
            }
            .into(),

            MenuItem {
                id,
                menu_type: MenuType::Standard,
                label: Some(label),
                enabled: _,
                visible: true,
                icon_name: _,
                icon_data,
                shortcut: _,
                toggle_type: _, // TODO: implement toggle
                toggle_state: _,
                children_display: _,
                disposition: _, // TODO: what to do with this?
                submenu: _,
            } => {
                let elem = tui::Elem::from(tui::Stack::horizontal([
                    tui::StackItem::spacing(depth + 1),
                    if let Some(icon) = icon_data
                        && let Some(img) =
                            image::load_from_memory_with_format(icon, image::ImageFormat::Png)
                                .context("Systray icon has invalid png data")
                                .ok_or_log()
                    {
                        let mut lines = label.lines();
                        let first_line = lines.next().unwrap_or_default();
                        tui::StackItem::auto(tui::Stack::vertical(Iterator::chain(
                            std::iter::once(tui::StackItem::length(
                                1,
                                tui::Stack::horizontal([
                                    tui::StackItem::auto(tui::Image {
                                        img: img.into_rgba8(),
                                        sizing: tui::ImageSizeMode::FillAxis(tui::Axis::Y, 1),
                                    }),
                                    tui::StackItem::spacing(1),
                                    tui::StackItem::auto(tui::PlainLines::new(first_line)),
                                ]),
                            )),
                            lines.map(tui::RawPrint::plain).map(tui::StackItem::auto),
                        )))
                    } else {
                        tui::StackItem::auto(tui::PlainLines::new(label))
                    },
                    tui::StackItem::spacing(1),
                ]));

                match on_interact {
                    Some(mk_interact) => elem.with_interact(mk_interact(*id)),
                    None => elem,
                }
            }

            _ => {
                log::error!("Unhandled menu item: {item:#?}");
                return None;
            }
        };

        Some(if item.submenu.is_empty() {
            main_elem
        } else {
            tui::Stack::vertical([
                tui::StackItem::auto(main_elem),
                tui::StackItem::auto(tray_menu_to_tui(depth + 1, &item.submenu, on_interact)),
            ])
            .into()
        })
    }

    fn tray_menu_to_tui(
        depth: u16,
        items: &[system_tray::menu::MenuItem],
        on_interact: Option<&impl Fn(i32) -> tui::InteractCallback>,
    ) -> tui::Elem {
        tui::Stack::vertical(items.iter().filter_map(|item| {
            Some(tui::StackItem {
                constr: tui::Constr::Auto,
                elem: tray_menu_item_to_tui(depth, item, on_interact)?,
            })
        }))
        .into()
    }
}

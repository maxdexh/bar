use std::{collections::HashMap, sync::Arc};

use anyhow::Context as _;
use bar_common::{
    tui,
    utils::{ReloadRx, ReloadTx, ResultExt as _, WatchRx, WatchTx, watch_chan},
};
use bar_panel_controller::BarTuiState;
use tokio::task::JoinSet;

use crate::clients;

#[derive(Clone, Debug)]
enum BarTuiElem {
    ByMonitor(HashMap<Arc<str>, tui::Elem>),
    Shared(tui::Elem),
    Hide,
    FillSpace(u16),
    Spacing(u16),
}
impl From<tui::Elem> for BarTuiElem {
    fn from(value: tui::Elem) -> Self {
        Self::Shared(value)
    }
}

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
    fn fixed(&mut self, elem: BarTuiElem) -> WatchRx<BarTuiElem> {
        let (_, rx) = watch_chan(elem);
        rx
    }
}

fn gather_bar_tui(bar_tui: &[BarTuiElem], tx: &WatchTx<BarTuiState>) {
    let mut by_monitor = HashMap::new();
    let mut fallback = tui::StackBuilder::new(tui::Axis::X);
    for elem in bar_tui {
        match elem {
            BarTuiElem::Shared(elem) => {
                for stack in by_monitor.values_mut().chain(Some(&mut fallback)) {
                    stack.fit(elem.clone());
                }
            }
            BarTuiElem::ByMonitor(elems) => {
                for (mtr, elem) in elems {
                    by_monitor
                        .entry(mtr.clone())
                        .or_insert_with(|| fallback.clone())
                        .fit(elem.clone());
                }
            }
            BarTuiElem::Hide => {}
            BarTuiElem::FillSpace(weight) => {
                for stack in by_monitor.values_mut().chain(Some(&mut fallback)) {
                    stack.fill(*weight, tui::Elem::empty());
                }
            }
            BarTuiElem::Spacing(len) => {
                for stack in by_monitor.values_mut().chain(Some(&mut fallback)) {
                    stack.spacing(*len);
                }
            }
        };
    }

    tx.send_replace(BarTuiState {
        by_monitor: by_monitor
            .into_iter()
            .map(|(k, stack)| (k, stack.build()))
            .collect(),
        fallback: fallback.build(),
    });
}

pub async fn main() -> std::process::ExitCode {
    let mut required_tasks = JoinSet::new();
    let mut reload_tx = ReloadTx::new();

    let bar_tui_tx = WatchTx::new(BarTuiState::default());
    required_tasks.spawn(bar_panel_controller::run_controller(
        bar_tui_tx.subscribe(),
        reload_tx.clone(),
    ));

    let mut fac = BarModuleFactory {
        reload_tx: reload_tx.clone(),
        tasks: JoinSet::new(),
    };

    let pulse = Arc::new(clients::pulse::connect(reload_tx.subscribe()));
    let mut modules = [
        fac.fixed(BarTuiElem::Spacing(1)),
        fac.spawn(hypr_module),
        fac.fixed(BarTuiElem::FillSpace(1)),
        fac.spawn(tray_module),
        fac.fixed(BarTuiElem::Spacing(3)),
        fac.spawn_with(
            PulseModuleCtx {
                pulse: pulse.clone(),
                device_kind: clients::pulse::PulseDeviceKind::Source,
                muted_sym: tui::RawPrint::plain(" ").into(),
                unmuted_sym: tui::RawPrint::center_symbol("", 2).into(),
            },
            pulse_module,
        ),
        fac.fixed(BarTuiElem::Spacing(3)),
        fac.spawn_with(
            PulseModuleCtx {
                pulse,
                device_kind: clients::pulse::PulseDeviceKind::Sink,
                muted_sym: tui::RawPrint::plain(" ").into(),
                unmuted_sym: tui::RawPrint::plain(" ").into(),
            },
            pulse_module,
        ),
        fac.fixed(BarTuiElem::Spacing(3)),
        fac.spawn(ppd_module),
        fac.spawn(energy_module),
        fac.fixed(BarTuiElem::Spacing(3)),
        fac.spawn(time_module),
    ];

    let mut module_tasks = JoinSet::new();

    {
        let bar_tui_tx_inner = WatchTx::new(Vec::from_iter(
            modules.iter_mut().map(|it| it.borrow_and_update().clone()),
        ));
        for (i, mut module) in modules.into_iter().enumerate() {
            let bar_tui_tx_inner = bar_tui_tx_inner.clone();
            module_tasks.spawn(async move {
                while let Ok(()) = module.changed().await {
                    let tui = module.borrow_and_update().clone();
                    bar_tui_tx_inner.send_modify(|modules| modules[i] = tui);
                }
            });
        }
        let mut bar_tui_rx_inner = bar_tui_tx_inner.subscribe();
        required_tasks.spawn(async move {
            while let Ok(()) = bar_tui_rx_inner.changed().await {
                gather_bar_tui(&bar_tui_rx_inner.borrow_and_update(), &bar_tui_tx);
            }
        });
    }

    reload_tx.reload();

    if let Some(res) = required_tasks.join_next().await {
        match res.ok_or_log() {
            Some(_) => std::process::ExitCode::SUCCESS,
            None => std::process::ExitCode::FAILURE,
        }
    } else {
        unreachable!()
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
            let wss = by_monitor
                .entry(monitor)
                .or_insert_with(|| tui::StackBuilder::new(tui::Axis::X));

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

            wss.fit(
                tui::Elem::from(tui::RawPrint::plain(&ws.name).styled(tui::Style {
                    fg: ws.is_active.then_some(tui::Color::Green),
                    ..Default::default()
                }))
                .on_interact(on_interact, None),
            );
            wss.spacing(1);
        }
        let by_monitor = by_monitor
            .into_iter()
            .map(|(k, v)| (k, v.build()))
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

    let on_interact = tui::InteractCallback::from_fn(move |interact| {
        if interact.kind != tui::InteractKind::Hover {
            return None;
        }

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

        // FIXME: Simplify
        let mut lines: Vec<_> = std::iter::repeat_n(
            std::array::repeat::<_, 7>(tui::Elem::empty().with_min_size(tui::Vec2 {
                x: 2,
                ..Default::default()
            })),
            num_weeks,
        )
        .collect();

        for n0 in 0u16..now.num_days_in_month().into() {
            let n1 = n0 + 1;
            let day = now
                .with_day(n1.into())
                .with_context(|| format!("Failed to set day {n1}"))
                .ok_or_log()?;

            let week_in_month = (first_day_offset + n0) / 7;
            let item = &mut lines[usize::from(week_in_month)][day.weekday() as usize];
            *item = {
                let it = tui::RawPrint::plain(format!("{n1:>2}"));
                if now == day {
                    it.styled(tui::Style {
                        fg: Some(tui::Color::Green),
                        ..Default::default()
                    })
                    .map_display(|styled| styled.to_string())
                    .into()
                } else {
                    it.into()
                }
            };
        }
        let weekday_line =
            ["Mo", "Tu", "We", "Th", "Fr", "Sa", "Su"].map(|d| tui::RawPrint::plain(d).into());

        let elem = tui::Elem::build_stack(tui::Axis::Y, |vstack| {
            vstack.fit(
                tui::RawPrint::plain(title)
                    .styled(tui::Style {
                        modifier: tui::Modifier {
                            bold: true,
                            ..Default::default()
                        },
                        ..Default::default()
                    })
                    .into(),
            );
            for line in std::iter::once(weekday_line).chain(lines) {
                vstack.fit(tui::Elem::build_stack(tui::Axis::X, |hstack| {
                    let mut first = true;
                    for day in line {
                        if !first {
                            hstack.spacing(1);
                        }
                        first = false;

                        hstack.fit(day);
                    }
                }));
            }
        });
        Some(tui::OpenMenu::tooltip(elem))
    });

    let mut prev_minutes = 61;
    loop {
        let now = chrono::Local::now();
        let minute = now.minute();
        if prev_minutes != minute {
            let tui = tui::RawPrint::plain(now.format("%H:%M %d/%m").to_string());
            tui_tx.send_replace(BarTuiElem::Shared(
                tui::Elem::from(tui).on_interact(&on_interact, None),
            ));

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
        drop(state);

        tui_tx.send_replace(BarTuiElem::Shared(
            tui::Elem::build_stack(tui::Axis::X, |stack| {
                stack.fit(if muted {
                    muted_sym.clone()
                } else {
                    unmuted_sym.clone()
                });
                stack.fit(
                    tui::RawPrint::plain(format!("{:>3}%", (volume * 100.0).round() as u32)).into(),
                );
            })
            .on_interact(&on_interact, None),
        ));
    }
}
async fn energy_module(
    ModuleArgs {
        tui_tx, reload_rx, ..
    }: ModuleArgs,
) {
    use crate::clients::upower::*;
    let energy = Arc::new(clients::upower::connect(reload_rx));

    let tooltip = tui::InteractCallback::from_fn_ctx(energy.clone(), |energy, interact| {
        if interact.kind != tui::InteractKind::Hover {
            return None;
        }
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
        Some(tui::OpenMenu::tooltip(tui::RawPrint::plain(text).into()))
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

        tui_tx.send_replace(BarTuiElem::Shared(
            tui::Elem::from(tui::RawPrint::plain(energy)).on_interact(&tooltip, None),
        ));
    }
}
async fn ppd_module(
    ModuleArgs {
        tui_tx, reload_rx, ..
    }: ModuleArgs,
) {
    let ppd = Arc::new(clients::ppd::connect(reload_rx));

    let on_interact =
        tui::InteractCallback::from_fn_ctx(ppd.clone(), |ppd, interact| match interact.kind {
            tui::InteractKind::Hover => {
                let profile = ppd.profile_rx.borrow().clone();
                Some(tui::OpenMenu::tooltip(
                    tui::PlainLines::new(profile.as_deref().unwrap_or("No profile")).into(),
                ))
            }
            tui::InteractKind::Click(tui::MouseButton::Left) => {
                ppd.cycle_profile();
                None
            }
            _ => None,
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
        let elem = elem.on_interact(&on_interact, None);

        tui_tx.send_replace(BarTuiElem::Shared(elem));
    }
}

async fn tray_module(
    ModuleArgs {
        tui_tx, reload_rx, ..
    }: ModuleArgs,
) {
    use crate::clients::tray::*;
    let tray = Arc::new(clients::tray::connect(reload_rx));

    fn interact_cb(
        (addr, tray): &(Arc<str>, Arc<TrayClient>),
        interact: tui::InteractArgs,
    ) -> Option<tui::OpenMenu> {
        match interact.kind {
            tui::InteractKind::Hover => {
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

                let header = tui::PlainLines::new(title).styled(tui::Style {
                    modifier: tui::Modifier {
                        bold: true,
                        ..Default::default()
                    },
                    ..Default::default()
                });
                let header = tui::Elem::build_stack(tui::Axis::X, |hstack| {
                    hstack.fill(1, tui::Elem::empty());
                    hstack.fit(header.into());
                    hstack.fill(1, tui::Elem::empty());
                });
                let tui = tui::Elem::build_stack(tui::Axis::Y, |vstack| {
                    vstack.fit(header);
                    vstack.fit(tui::PlainLines::new(description).into());
                });
                Some(tui::OpenMenu::tooltip(tui))
            }
            tui::InteractKind::Click(tui::MouseButton::Right) => {
                let TrayMenuExt {
                    menu_path,
                    submenus,
                    ..
                } = tray
                    .state_rx
                    .borrow()
                    .menus
                    .get(addr)
                    .cloned()
                    .with_context(|| format!("Unknown tray addr {addr}"))
                    .ok_or_log()?;

                let icb = |menu_path: &Arc<str>, id| {
                    let tray = tray.clone();
                    let addr = addr.clone();
                    let menu_path = menu_path.clone();
                    tui::InteractCallback::from_fn(move |interact| {
                        if let tui::InteractKind::Click(tui::MouseButton::Left) = interact.kind {
                            let addr = addr.clone();
                            let menu_path = menu_path.clone();
                            tray.sched_with_client(async move |client| {
                                client
                                    .activate(system_tray::client::ActivateRequest::MenuItem {
                                        address: str::to_owned(&addr),
                                        menu_path: str::to_owned(&menu_path),
                                        submenu_id: id,
                                    })
                                    .await
                                    .context("Failed to send ActivateRequest")
                                    .ok_or_log();
                            });
                        }
                        None
                    })
                };

                let menu_tui = tray_menu_to_tui(
                    0,
                    &submenus,
                    menu_path
                        .as_ref()
                        .map(|menu_path| |id| icb(menu_path, id))
                        .as_ref(),
                );
                Some(tui::OpenMenu::context(tui::Elem::build_block(|block| {
                    block.set_borders_at(tui::Borders::all());
                    block.set_style(tui::Style {
                        fg: Some(tui::Color::DarkGrey),
                        ..Default::default()
                    });
                    block.set_lines(tui::LineSet::thick());
                    block.set_inner(menu_tui);
                })))
            }
            _ => None,
        }
    }

    let mut state_rx = tray.state_rx.clone();
    while state_rx.changed().await.is_ok() {
        let items = state_rx.borrow_and_update().items.clone();

        let tui = tui::Elem::build_stack(tui::Axis::X, |stack| {
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

                    stack.fit(
                        tui::Elem::image(
                            img, //
                            tui::ImageSizeMode::FillAxis(tui::Axis::Y, 1),
                        )
                        .on_interact(
                            tui::InteractCallback::from_fn_ctx(
                                (addr.clone(), tray.clone()),
                                interact_cb,
                            ),
                            None,
                        ),
                    );
                    stack.spacing(1);
                }
            }
        });
        tui_tx.send_replace(BarTuiElem::Shared(tui));
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
            } => tui::Elem::build_block(|block| {
                block.set_borders_at(tui::Borders {
                    top: true,
                    ..Default::default()
                });
                block.set_style(tui::Style {
                    fg: Some(tui::Color::DarkGrey),
                    ..Default::default()
                });
            }),
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
                let elem = tui::Elem::build_stack(tui::Axis::X, |stack| {
                    stack.spacing(depth + 1);
                    if let Some(icon) = icon_data
                        && let Some(img) =
                            image::load_from_memory_with_format(icon, image::ImageFormat::Png)
                                .context("Systray icon has invalid png data")
                                .ok_or_log()
                    {
                        stack.fit(tui::Elem::image(
                            img.into_rgba8(),
                            tui::ImageSizeMode::FillAxis(tui::Axis::Y, 1),
                        ));
                        stack.spacing(1);
                    }
                    stack.fit(tui::PlainLines::new(label).into())
                });

                match on_interact {
                    Some(mk_interact) => elem.on_interact(mk_interact(*id), None),
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
            tui::Elem::build_stack(tui::Axis::Y, |stack| {
                stack.fit(main_elem);
                stack.fit(tray_menu_to_tui(depth + 1, &item.submenu, on_interact));
            })
        })
    }

    fn tray_menu_to_tui(
        depth: u16,
        items: &[system_tray::menu::MenuItem],
        on_interact: Option<&impl Fn(i32) -> tui::InteractCallback>,
    ) -> tui::Elem {
        tui::Elem::build_stack(tui::Axis::Y, |stack| {
            for item in items {
                if let Some(item) = tray_menu_item_to_tui(depth, item, on_interact) {
                    stack.fit(item)
                }
            }
        })
    }
}

use crate::tui::MouseButton;
use crate::{
    modules::{self, tray::TrayState},
    procs::{
        bar_panel::{BarEvent, BarEventInfo, BarUpdate},
        menu_panel::{Menu, MenuEvent, MenuUpdate},
    },
    tui,
    utils::{Emit, ReloadTx, unb_chan},
};
use futures::Stream;
use tokio::task::JoinSet;
use tokio_stream::StreamExt as _;

async fn dump_stream<T>(mut dst: impl Emit<T>, src: impl Stream<Item = T>) {
    tokio::pin!(src);
    while let Some(item) = src.next().await {
        if let Err(err) = dst.try_emit(item) {
            log::warn!("{err}");
            break;
        }
    }
}

// TODO: Draw on the controller, send rendered buffer to each panel
// TODO: Add network module
// TODO: Middle click to open related settings
pub async fn main() {
    log::debug!("Starting controller");

    let mut client_tasks = JoinSet::<()>::new();
    let mut important_tasks = JoinSet::<()>::new(); // FIXME: Exit on mgr exit

    let reload_tx = ReloadTx::new();
    let reload_rx = reload_tx.subscribe();

    let (mut bar_upd_tx, bar_ev_rx);
    let (mut menu_upd_tx, menu_ev_rx);
    {
        let (mut bar_monitor_tx, bar_monitor_rx) = unb_chan();
        let (mut menu_monitor_tx, menu_monitor_rx) = unb_chan();

        let (bar_ev_tx, bar_upd_rx);
        let (menu_ev_tx, menu_upd_rx);

        (bar_upd_tx, bar_upd_rx) = unb_chan();
        (bar_ev_tx, bar_ev_rx) = unb_chan();
        (menu_upd_tx, menu_upd_rx) = unb_chan();
        (menu_ev_tx, menu_ev_rx) = unb_chan();

        crate::monitors::connect(move |ev: crate::monitors::MonitorEvent| {
            let f1 = bar_monitor_tx.try_emit(ev.clone());
            let f2 = menu_monitor_tx.try_emit(ev);
            f1?;
            f2
        });
        important_tasks.spawn(crate::procs::bar_panel::run_bar_panel_manager(
            bar_monitor_rx,
            bar_upd_rx,
            bar_ev_tx,
        ));
        important_tasks.spawn(crate::procs::menu_panel::run_menu_panel_manager(
            menu_monitor_rx,
            menu_upd_rx,
            menu_ev_tx,
        ));
    };

    client_tasks.spawn(dump_stream(
        bar_upd_tx.clone(),
        tokio_stream::wrappers::WatchStream::new(modules::hypr::connect(reload_rx.resubscribe()))
            .map(BarUpdate::Desktop),
    ));
    client_tasks.spawn(dump_stream(
        bar_upd_tx.clone(),
        modules::upower::connect(reload_rx.resubscribe()).map(BarUpdate::Energy),
    ));
    client_tasks.spawn(dump_stream(
        bar_upd_tx.clone(),
        modules::time::connect(reload_rx.resubscribe()).map(BarUpdate::Time),
    ));

    enum Upd {
        Tray(TrayState),
        Bar(BarEventInfo, BarEvent),
        Menu(MenuEvent),
    }
    let (tray_tx, tray_rx) = {
        let (tx, stream) = modules::tray::connect(reload_rx.resubscribe());
        (tx, stream.map(Upd::Tray))
    };
    let mut ppd_switch_tx = {
        let (tx, profiles) = modules::ppd::connect(reload_rx.resubscribe());
        client_tasks.spawn(dump_stream(
            bar_upd_tx.clone(),
            profiles.map(BarUpdate::Ppd),
        ));
        tx
    };
    let mut audio_upd_tx = {
        let (tx, events) = modules::pulse::connect(reload_rx.resubscribe());
        client_tasks.spawn(dump_stream(
            bar_upd_tx.clone(),
            events.map(BarUpdate::Pulse),
        ));
        tx
    };

    // TODO: Try to parallelize this further.
    let updates = tray_rx
        .merge(bar_ev_rx.map(|(info, ev)| Upd::Bar(info, ev)))
        .merge(menu_ev_rx.map(Upd::Menu));
    tokio::pin!(updates);

    let mut tray_state = TrayState::default();
    while let Some(controller_update) = updates.next().await {
        // NOTE: The clients' senders should never be closed here, since their
        // listeners are being listened to. If they are, it indicates an error in the program.
        // Note that the panels' senders may actually be closed, which just indicates that
        // no panel is visible at the moment. The error message 'channel closed' is misleading
        // in that case.
        match controller_update {
            Upd::Tray(state) => {
                bar_upd_tx.emit(BarUpdate::SysTray(state.items.clone()));
                tray_state = state;
            }

            Upd::Bar(
                BarEventInfo { monitor },
                BarEvent::Interact(tui::InteractGeneric {
                    location,
                    payload: target,
                    kind,
                }),
            ) => {
                use crate::procs::bar_panel::BarInteractTarget as IT;
                use crate::tui::InteractKind as IK;

                let mkswitch = |new_menu| MenuUpdate::SwitchSubject {
                    new_menu,
                    location,
                    monitor: monitor.clone(),
                };

                match (&kind, target) {
                    (IK::Hover, IT::Tray(addr)) => match tray_state
                        .items
                        .iter()
                        .find(|(a, _)| a == &addr)
                        .and_then(|(_, item)| item.tool_tip.as_ref())
                    {
                        Some(tt) => menu_upd_tx.emit(mkswitch(Menu::TrayTooltip {
                            addr,
                            tooltip: tt.clone(),
                        })),
                        None => menu_upd_tx.emit(MenuUpdate::UnfocusMenu),
                    },

                    (IK::Click(MouseButton::Right), IT::Tray(addr)) => {
                        menu_upd_tx.emit(match tray_state.menus.iter().find(|(a, _)| a == &addr) {
                            Some((_, menu)) => mkswitch(Menu::TrayContext {
                                addr,
                                tmenu: menu.clone(),
                            }),
                            None => MenuUpdate::Hide,
                        })
                    }

                    (IK::Click(MouseButton::Left), IT::Ppd) => {
                        ppd_switch_tx.emit(modules::ppd::CycleProfile);
                    }
                    (IK::Click(MouseButton::Left), IT::Audio(target)) => {
                        audio_upd_tx.emit(modules::pulse::PulseUpdate {
                            target,
                            kind: modules::pulse::PulseUpdateKind::ToggleMute,
                        });
                    }
                    (IK::Click(MouseButton::Right), IT::Audio(target)) => {
                        audio_upd_tx.emit(modules::pulse::PulseUpdate {
                            target,
                            kind: modules::pulse::PulseUpdateKind::ResetVolume,
                        });
                    }
                    (IK::Scroll(direction), IT::Audio(target)) => {
                        audio_upd_tx.emit(modules::pulse::PulseUpdate {
                            target,
                            kind: modules::pulse::PulseUpdateKind::VolumeDelta(
                                2 * match direction {
                                    tui::Direction::Up => 1,
                                    tui::Direction::Down => -1,
                                    tui::Direction::Left => -1,
                                    tui::Direction::Right => 1,
                                },
                            ),
                        });
                    }

                    // TODO: Implement more interactions
                    (IK::Hover, _) => menu_upd_tx.emit(MenuUpdate::UnfocusMenu),
                    (IK::Click(_) | IK::Scroll(_), _) => menu_upd_tx.emit(MenuUpdate::Hide),
                }
            }
            Upd::Menu(menu) => match menu {
                MenuEvent::Interact(tui::InteractGeneric {
                    location: _,
                    payload: target,
                    kind,
                }) => match target {
                    #[expect(clippy::single_match)]
                    crate::procs::menu_panel::MenuInteractTarget::TrayMenu(interact) => {
                        match kind {
                            tui::InteractKind::Click(_) => {
                                if let Err(err) = tray_tx.send(interact) {
                                    log::error!("Failed to send interaction: {err}");
                                }
                            }
                            _ => (),
                        }
                    }
                },
            },
        }
    }
}

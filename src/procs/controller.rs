use std::ops::ControlFlow;

use crate::{
    clients::{self, tray::TrayState},
    data::InteractGeneric,
    procs::{
        bar_panel::{BarEvent, BarEventInfo, BarUpdate},
        menu_panel::{Menu, MenuEvent, MenuUpdate},
    },
    utils::{Emit, ReloadRx, dump_stream, unb_chan},
};
use crossterm::event::MouseButton;
use tokio::task::JoinSet;
use tokio_stream::StreamExt as _;

// TODO: Draw on the controller, send rendered buffer to each panel
// TODO: Add network module
// TODO: Middle click to open related settings
pub async fn main() {
    log::debug!("Starting controller");

    let mut client_tasks = JoinSet::<()>::new();
    let mut important_tasks = JoinSet::<()>::new(); // FIXME: Exit on mgr exit

    let (reload_tx, reload_rx) = ReloadRx::new();

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
            let f1 = bar_monitor_tx.emit(ev.clone());
            let f2 = menu_monitor_tx.emit(ev);
            reload_tx();
            if f1.is_break() || f2.is_break() {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
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
        clients::hypr::connect(reload_rx.resubscribe()).map(BarUpdate::Desktop),
    ));
    client_tasks.spawn(dump_stream(
        bar_upd_tx.clone(),
        clients::upower::connect(reload_rx.resubscribe()).map(BarUpdate::Energy),
    ));
    client_tasks.spawn(dump_stream(
        bar_upd_tx.clone(),
        clients::clock::connect(reload_rx.resubscribe()).map(BarUpdate::Time),
    ));

    enum Upd {
        Tray(TrayState),
        Bar(BarEventInfo, BarEvent),
        Menu(MenuEvent),
    }
    let (tray_tx, tray_rx) = {
        let (tx, stream) = clients::tray::connect(reload_rx.resubscribe());
        (tx, stream.map(Upd::Tray))
    };
    let ppd_switch_tx = {
        let (tx, profiles) = clients::ppd::connect(reload_rx.resubscribe());
        client_tasks.spawn(dump_stream(
            bar_upd_tx.clone(),
            profiles.map(BarUpdate::Ppd),
        ));
        tx
    };
    let mut audio_upd_tx = {
        let (tx, events) = clients::pulse::connect(reload_rx.resubscribe());
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
                if bar_upd_tx
                    .emit(BarUpdate::SysTray(state.items.clone()))
                    .is_break()
                {
                    break;
                }
                tray_state = state;
            }

            Upd::Bar(
                BarEventInfo { monitor },
                BarEvent::Interact(InteractGeneric {
                    location,
                    target,
                    kind,
                }),
            ) => {
                use crate::data::InteractKind as IK;
                use crate::procs::bar_panel::BarInteractTarget as IT;

                let mkswitch = |new_menu| MenuUpdate::SwitchSubject {
                    new_menu,
                    location,
                    monitor: monitor.clone(),
                };

                let flow = match (&kind, target) {
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
                        if let Err(err) = ppd_switch_tx.send(()) {
                            log::error!("Failed to send profile switch: {err}")
                        }
                        ControlFlow::Continue(())
                    }
                    (IK::Click(MouseButton::Left), IT::Audio(target)) => {
                        if audio_upd_tx
                            .emit(clients::pulse::PulseUpdate {
                                target,
                                kind: clients::pulse::PulseUpdateKind::ToggleMute,
                            })
                            .is_break()
                        {
                            // TODO: Restart client
                        }
                        ControlFlow::Continue(())
                    }
                    (IK::Click(MouseButton::Right), IT::Audio(target)) => {
                        if audio_upd_tx
                            .emit(clients::pulse::PulseUpdate {
                                target,
                                kind: clients::pulse::PulseUpdateKind::ResetVolume,
                            })
                            .is_break()
                        {
                            // TODO: Restart client
                        }
                        ControlFlow::Continue(())
                    }
                    (IK::Scroll(direction), IT::Audio(target)) => {
                        if audio_upd_tx
                            .emit(clients::pulse::PulseUpdate {
                                target,
                                kind: clients::pulse::PulseUpdateKind::VolumeDelta(
                                    2 * match direction {
                                        crate::data::Direction::Up => 1,
                                        crate::data::Direction::Down => -1,
                                        crate::data::Direction::Left => -1,
                                        crate::data::Direction::Right => 1,
                                    },
                                ),
                            })
                            .is_break()
                        {
                            // TODO: Restart client
                        }
                        ControlFlow::Continue(())
                    }

                    // TODO: Implement more interactions
                    (IK::Hover, _) => menu_upd_tx.emit(MenuUpdate::UnfocusMenu),
                    (IK::Click(_) | IK::Scroll(_), _) => menu_upd_tx.emit(MenuUpdate::Hide),
                };
                if flow.is_break() {
                    break;
                }
            }
            Upd::Menu(menu) => match menu {
                MenuEvent::Interact(InteractGeneric {
                    location: _,
                    target,
                    kind,
                }) => match target {
                    #[expect(clippy::single_match)]
                    crate::procs::menu_panel::MenuInteractTarget::TrayMenu(interact) => {
                        match kind {
                            crate::data::InteractKind::Click(_) => {
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

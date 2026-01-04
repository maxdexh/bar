use std::sync::Arc;

use crate::{
    clients::{self, tray::TrayState},
    data::InteractGeneric,
    procs::{
        bar_panel::{BarEvent, BarUpdate},
        menu_panel::{Menu, MenuEvent, MenuUpdate},
    },
    utils::{BasicTaskMap, Emit, ReloadRx, broadcast_stream, dump_stream, ubchan},
};
use anyhow::Result;
use crossterm::event::MouseButton;
use tokio::{
    sync::{broadcast, mpsc},
    task::JoinSet,
};
use tokio_stream::StreamExt as _;

async fn into_send_all<T: serde::Serialize + Send + 'static>(
    tx: broadcast::Sender<Arc<T>>,
    stream: impl futures::Stream<Item = T>,
) {
    tokio::pin!(stream);
    while let Some(upd) = stream.next().await {
        send_update(&tx, upd)
    }
}

#[track_caller]
fn send_update<T>(tx: &broadcast::Sender<Arc<T>>, upd: T) {
    if let Err(err) = tx.send(Arc::new(upd)) {
        log::warn!("Failed to send update: {err}")
    }
}

// TODO: Draw on the controller, send rendered buffer to each panel
// TODO: Add network module
// TODO: Middle click to open related settings
pub async fn main() -> Result<()> {
    log::debug!("Starting controller");

    let mut subtasks = JoinSet::<()>::new();

    let (do_reload, reload_rx) = ReloadRx::new();

    let (mut bar_upd_send, bar_events, menu_tx, menu_events) = {
        let menu_upd_tx = broadcast::Sender::<Arc<MenuUpdate>>::new(1000);

        // Channel to receive events from panels. Messages are collected
        // into an unbounded channel to ensure that we do not block the
        // sockets.
        let (menu_ev_tx, mut menu_ev_rx) = mpsc::unbounded_channel();

        //let (bar_send, bar_events) = crate::procs::bar_panel::control_panels(
        //    &mut subtasks,
        //    bar_ui_tx.clone(),
        //    bar_panel_ev_rx,
        //);
        let (monitor_tx, monitor_rx) = ubchan();
        let (bar_send, bar_upd_rx) = ubchan();
        let (bar_ev_tx, bar_events) = ubchan();
        clients::monitors::connect(monitor_tx);
        subtasks.spawn(crate::procs::bar_panel::run_bar_panel_manager(
            monitor_rx, bar_upd_rx, bar_ev_tx,
        ));

        let menu_upd_tx_clone = menu_upd_tx.clone();
        subtasks.spawn(async move {
            let tasks = BasicTaskMap::new();

            let mut display_diffs = clients::displays::connect();

            while let Some(clients::displays::DisplayDiff { added, removed }) =
                display_diffs.next().await
            {
                for display in removed {
                    tasks.cancel(&display);
                }

                let should_reload = !added.is_empty();
                for display in added {
                    let menu_upd_rx = menu_upd_tx_clone.subscribe();
                    let menu_ev_tx = menu_ev_tx.clone();
                    tasks.insert_spawn(display.clone(), async move {
                        let mut panels = JoinSet::<Result<_, _>>::new();
                        panels.spawn(super::run_panel_controller_side::<_, MenuUpdate>(
                            "menu-panel.sock",
                            display.clone(),
                            menu_ev_tx,
                            broadcast_stream(menu_upd_rx),
                            super::menu_panel::controller_spawn_panel,
                        ));
                        if let Some(Err(err)) = panels.join_next().await {
                            log::error!("{err}");
                        }
                        panels.abort_all();
                    });
                }
                if should_reload {
                    do_reload();
                }
            }
        });

        (
            bar_send,
            bar_events,
            menu_upd_tx,
            futures::stream::poll_fn(move |cx| menu_ev_rx.poll_recv(cx)),
        )
    };

    {
        let (ws, am) = clients::hypr::connect(reload_rx.resubscribe());
        subtasks.spawn(dump_stream(
            bar_upd_send.clone(),
            ws.map(BarUpdate::Desktop),
        ));
        subtasks.spawn(into_send_all(
            menu_tx.clone(),
            am.map(MenuUpdate::ActiveMonitor),
        ));
    }
    subtasks.spawn(dump_stream(
        bar_upd_send.clone(),
        clients::upower::connect(reload_rx.resubscribe()).map(BarUpdate::Energy),
    ));
    subtasks.spawn(dump_stream(
        bar_upd_send.clone(),
        clients::clock::connect(reload_rx.resubscribe()).map(BarUpdate::Time),
    ));

    type TrayEvent = system_tray::client::Event;
    enum Upd {
        Tray(TrayEvent, TrayState),
        Bar(BarEvent),
        Menu(MenuEvent),
    }
    let (tray_tx, tray_stream) = {
        let (tx, stream) = clients::tray::connect(reload_rx.resubscribe());
        (tx, stream.map(|(event, state)| Upd::Tray(event, state)))
    };
    let ppd_switch_tx = {
        let (tx, profiles) = clients::ppd::connect(reload_rx.resubscribe());
        subtasks.spawn(dump_stream(
            bar_upd_send.clone(),
            profiles.map(BarUpdate::Ppd),
        ));
        tx
    };
    let audio_upd_tx = {
        let (tx, events) = clients::pulse::connect(reload_rx.resubscribe());
        subtasks.spawn(dump_stream(
            bar_upd_send.clone(),
            events.map(BarUpdate::Pulse),
        ));
        tx
    };

    // TODO: Try to parallelize this further.
    let big_stream = tray_stream
        .merge(bar_events.map(|(_info, ev)| ev).map(Upd::Bar)) // FIXME: Use info
        .merge(menu_events.map(Upd::Menu));
    tokio::pin!(big_stream);

    let mut tray_state = TrayState::default();
    while let Some(controller_update) = big_stream.next().await {
        // NOTE: The clients' senders should never be closed here, since their
        // listeners are being listened to. If they are, it indicates an error in the program.
        // Note that the panels' senders may actually be closed, which just indicates that
        // no panel is visible at the moment. The error message 'channel closed' is misleading
        // in that case.
        match controller_update {
            Upd::Tray(event, state) => {
                if bar_upd_send
                    .emit(BarUpdate::SysTray(state.items.clone()))
                    .is_break()
                {
                    break;
                }
                tray_state = state;

                match event {
                    TrayEvent::Add(_, _) => (),
                    TrayEvent::Update(addr, event) => match event {
                        system_tray::client::UpdateEvent::Tooltip(tooltip) => {
                            send_update(
                                &menu_tx,
                                MenuUpdate::UpdateTrayTooltip(addr.into(), tooltip),
                            );
                        }
                        system_tray::client::UpdateEvent::Menu(_)
                        | system_tray::client::UpdateEvent::MenuDiff(_) => {
                            match tray_state
                                .menus
                                .iter()
                                .find(|(a, _)| a as &str == addr.as_str())
                            {
                                Some((_, menu)) => {
                                    send_update(
                                        &menu_tx,
                                        MenuUpdate::UpdateTrayMenu(addr.into(), menu.clone()),
                                    );
                                }
                                None => {
                                    log::error!("Got update for non-existent menu '{addr}'?")
                                }
                            }
                        }
                        system_tray::client::UpdateEvent::MenuConnect(menu) => {
                            send_update(
                                &menu_tx,
                                MenuUpdate::ConnectTrayMenu {
                                    addr,
                                    menu_path: Some(menu), // TODO: Send removals too
                                },
                            );
                        }
                        _ => (),
                    },
                    system_tray::client::Event::Remove(addr) => {
                        send_update(&menu_tx, MenuUpdate::RemoveTray(addr.into()));
                    }
                }
            }

            Upd::Bar(BarEvent::Interact(InteractGeneric {
                location,
                target,
                kind,
            })) => {
                use crate::data::InteractKind as IK;
                use crate::procs::bar_panel::BarInteractTarget as IT;

                let send_menu = |menu| {
                    send_update(
                        &menu_tx,
                        MenuUpdate::SwitchSubject {
                            new_menu: menu,
                            location,
                        },
                    );
                };
                let unfocus_menu = || {
                    send_update(&menu_tx, MenuUpdate::UnfocusMenu);
                };

                let default_action = || match kind {
                    IK::Hover => unfocus_menu(),
                    IK::Click(_) | IK::Scroll(_) => send_menu(Menu::None),
                };

                match (&kind, target) {
                    (IK::Hover, IT::Tray(addr)) => match tray_state
                        .items
                        .iter()
                        .find(|(a, _)| a == &addr)
                        .and_then(|(_, item)| item.tool_tip.as_ref())
                    {
                        Some(tt) => send_menu(Menu::TrayTooltip {
                            addr,
                            tooltip: tt.clone(),
                        }),
                        None => unfocus_menu(),
                    },

                    (IK::Click(MouseButton::Right), IT::Tray(addr)) => {
                        send_menu(match tray_state.menus.iter().find(|(a, _)| a == &addr) {
                            Some((_, menu)) => Menu::TrayContext {
                                addr,
                                tmenu: menu.clone(),
                            },
                            None => Menu::None,
                        })
                    }

                    (IK::Click(MouseButton::Left), IT::Ppd) => {
                        if let Err(err) = ppd_switch_tx.send(()) {
                            log::error!("Failed to send profile switch: {err}")
                        }
                    }
                    (IK::Click(MouseButton::Left), IT::Audio(target)) => {
                        if let Err(err) = audio_upd_tx.send(clients::pulse::PulseUpdate {
                            target,
                            kind: clients::pulse::PulseUpdateKind::ToggleMute,
                        }) {
                            log::error!("Failed to send audio update: {err}")
                        }
                    }
                    (IK::Click(MouseButton::Right), IT::Audio(target)) => {
                        if let Err(err) = audio_upd_tx.send(clients::pulse::PulseUpdate {
                            target,
                            kind: clients::pulse::PulseUpdateKind::ResetVolume,
                        }) {
                            log::error!("Failed to send audio update: {err}")
                        }
                    }
                    (IK::Scroll(direction), IT::Audio(target)) => {
                        if let Err(err) = audio_upd_tx.send(clients::pulse::PulseUpdate {
                            target,
                            kind: clients::pulse::PulseUpdateKind::VolumeDelta(
                                2 * match direction {
                                    crate::data::Direction::Up => 1,
                                    crate::data::Direction::Down => -1,
                                    crate::data::Direction::Left => -1,
                                    crate::data::Direction::Right => 1,
                                },
                            ),
                        }) {
                            log::error!("Failed to send audio update: {err}")
                        }
                    }

                    // TODO: Implement more interactions
                    _ => default_action(),
                };
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
                MenuEvent::Watcher(ev) => {
                    send_update(&menu_tx, MenuUpdate::Watcher(ev));
                }
            },
        }
    }

    unreachable!()
}

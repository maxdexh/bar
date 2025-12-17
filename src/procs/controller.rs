use std::{process::ExitStatus, sync::Arc};

use crate::{
    clients::{self, tray::TrayState},
    data::InteractGeneric,
    procs::{
        bar_panel::{BarEvent, BarUpdate},
        menu_panel::{Menu, MenuEvent, MenuUpdate, MenuWatcherEvents},
    },
    utils::{IpcReceiver, IpcSender, ResultExt as _},
};
use anyhow::{Result, anyhow};
use crossterm::event::MouseButton;
use futures::{FutureExt, Stream};
use tokio::task::JoinSet;
use tokio_stream::StreamExt as _;

async fn into_send_all<T: serde::Serialize + Send + 'static>(
    tx: Arc<IpcSender<T>>,
    stream: impl futures::Stream<Item = T>,
) {
    tokio::pin!(stream);
    while let Some(value) = stream.next().await {
        tx.send_or_log(value).await;
    }
}

pub async fn main(
    bar_tx: IpcSender<BarUpdate>,
    bar_rx: IpcReceiver<BarEvent>,
    menu_tx: IpcSender<MenuUpdate>,
    menu_rx: IpcReceiver<MenuEvent>,
    menu_watcher_evs: impl Send + 'static + Stream<Item = MenuWatcherEvents>,
    mut bar_proc: tokio::process::Child,
    mut menu_proc: tokio::process::Child,
) -> Result<()> {
    log::debug!("Starting controller");

    let bar_tx = Arc::new(bar_tx);
    let menu_tx = Arc::new(menu_tx);

    let mut subtasks = JoinSet::new();
    {
        let (ws, am) = clients::hypr::subscribe();
        subtasks.spawn(into_send_all(bar_tx.clone(), ws.map(BarUpdate::Desktop)));
        subtasks.spawn(into_send_all(
            menu_tx.clone(),
            am.map(MenuUpdate::ActiveMonitor),
        ));
    }
    subtasks.spawn(into_send_all(
        bar_tx.clone(),
        clients::upower::connect().map(BarUpdate::Energy),
    ));
    subtasks.spawn(into_send_all(
        bar_tx.clone(),
        clients::clock::connect().map(BarUpdate::Time),
    ));
    subtasks.spawn(into_send_all(
        menu_tx.clone(),
        menu_watcher_evs.map(MenuUpdate::Watcher),
    ));

    type TrayEvent = system_tray::client::Event;
    enum Upd {
        Tray(TrayEvent, TrayState),
        Bar(BarEvent),
        Menu(MenuEvent),
        BarExit(std::io::Result<ExitStatus>),
        MenuExit(std::io::Result<ExitStatus>),
    }
    let (tray_tx, tray_stream) = {
        let (tx, stream) = clients::tray::connect();
        (tx, stream.map(|(event, state)| Upd::Tray(event, state)))
    };
    let ppd_switch_tx = {
        let (tx, profiles) = clients::ppd::connect();
        subtasks.spawn(into_send_all(bar_tx.clone(), profiles.map(BarUpdate::Ppd)));
        tx
    };
    let audio_upd_tx = {
        let (tx, events) = clients::pulse::connect();
        subtasks.spawn(into_send_all(bar_tx.clone(), events.map(BarUpdate::Pulse)));
        tx
    };

    // TODO: Handle multiple monitors by putting multiple bars in a StreamMap
    let bar_exit = bar_proc.wait().into_stream().map(Upd::BarExit);
    let menu_exit = menu_proc.wait().into_stream().map(Upd::MenuExit);

    // TODO: Try to parallelize this further.
    let big_stream = tray_stream
        .merge(bar_rx.into_stream().map(Upd::Bar))
        .merge(menu_rx.into_stream().map(Upd::Menu))
        .merge(bar_exit)
        .merge(menu_exit);
    tokio::pin!(big_stream);

    let mut tray_state = TrayState::default();
    while let Some(controller_update) = big_stream.next().await {
        match controller_update {
            Upd::Tray(event, state) => {
                bar_tx // FIXME: unnecessary clone
                    .send_or_log(BarUpdate::SysTray(state.items.clone()))
                    .await;
                tray_state = state;

                match event {
                    TrayEvent::Add(_, _) => (),
                    TrayEvent::Update(addr, event) => match event {
                        system_tray::client::UpdateEvent::Tooltip(tooltip) => {
                            menu_tx
                                .send_or_log(MenuUpdate::UpdateTrayTooltip(addr.into(), tooltip))
                                .await
                        }
                        system_tray::client::UpdateEvent::Menu(_)
                        | system_tray::client::UpdateEvent::MenuDiff(_) => {
                            match tray_state.menus.get(addr.as_str()) {
                                Some(menu) => {
                                    menu_tx
                                        .send_or_log(MenuUpdate::UpdateTrayMenu(
                                            addr.into(),
                                            menu.clone(),
                                        ))
                                        .await
                                }
                                None => {
                                    log::error!("Got update for non-existent menu '{addr}'?")
                                }
                            }
                        }
                        system_tray::client::UpdateEvent::MenuConnect(menu) => {
                            menu_tx
                                .send_or_log(MenuUpdate::ConnectTrayMenu {
                                    addr,
                                    menu_path: Some(menu), // TODO: Send removals too
                                })
                                .await
                        }
                        _ => (),
                    },
                    system_tray::client::Event::Remove(addr) => {
                        menu_tx
                            .send_or_log(MenuUpdate::RemoveTray(addr.into()))
                            .await
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

                let send_menu = async |menu| {
                    menu_tx
                        .send(MenuUpdate::SwitchSubject { menu, location })
                        .await
                        .ok_or_log("Failed to send bar interaction to menu");
                };
                let unfocus_menu = async || {
                    menu_tx
                        .send(MenuUpdate::UnfocusMenu)
                        .await
                        .ok_or_log("Failed to send bar interaction to menu");
                };

                let default_action = async || match kind {
                    IK::Hover => unfocus_menu().await,
                    IK::Click(_) | IK::Scroll(_) => send_menu(Menu::None).await,
                };

                match (&kind, target) {
                    (IK::Hover, IT::Tray(addr)) => match tray_state
                        .items
                        .get(addr.as_ref())
                        .and_then(|item| item.tool_tip.as_ref())
                    {
                        Some(tt) => {
                            send_menu(Menu::TrayTooltip {
                                addr,
                                tooltip: tt.clone(),
                            })
                            .await
                        }
                        None => unfocus_menu().await,
                    },

                    (IK::Click(MouseButton::Right), IT::Tray(addr)) => {
                        send_menu(match tray_state.menus.get(addr.as_ref()) {
                            Some(menu) => Menu::TrayContext {
                                addr,
                                tmenu: menu.clone(),
                            },
                            None => Menu::None,
                        })
                        .await
                    }

                    (IK::Click(MouseButton::Left), IT::Ppd) => {
                        ppd_switch_tx
                            .send(())
                            .ok_or_log("Failed to send profile switch");
                    }

                    (IK::Click(MouseButton::Left), IT::Audio(target)) => {
                        audio_upd_tx
                            .send(clients::pulse::PulseUpdate {
                                target,
                                kind: clients::pulse::PulseUpdateKind::ToggleMute,
                            })
                            .await
                            .ok_or_log("Failed to send audio update");
                    }
                    (IK::Click(MouseButton::Right), IT::Audio(target)) => {
                        audio_upd_tx
                            .send(clients::pulse::PulseUpdate {
                                target,
                                kind: clients::pulse::PulseUpdateKind::ResetVolume,
                            })
                            .await
                            .ok_or_log("Failed to send audio update");
                    }
                    (IK::Scroll(direction), IT::Audio(target)) => {
                        audio_upd_tx
                            .send(clients::pulse::PulseUpdate {
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
                            .await
                            .ok_or_log("Failed to send audio update");
                    }

                    // TODO: Implement more interactions
                    _ => default_action().await,
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
                                tray_tx
                                    .send(interact)
                                    .await
                                    .ok_or_log("Failed to send interaction");
                            }
                            _ => (),
                        }
                    }
                },
            },

            Upd::MenuExit(menu_exit) => {
                _ = menu_exit?;
                return Err(anyhow!("Context menu exited"));
            }
            // FIXME: Exiting is expected once multiple monitors are supported. We need to handle
            // this by removing a stream from a streammap, or similar
            Upd::BarExit(bar_exit) => {
                _ = bar_exit?;
                return Err(anyhow!("Bar exited"));
            }
        }
    }

    unreachable!()
}

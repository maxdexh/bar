use crate::data::{BasicDesktopState, BasicWorkspace, WorkspaceId};
use crate::modules::prelude::*;
use crate::tui;
use crate::utils::Emit;
use crate::utils::{ReloadRx, ResultExt, WatchRx, lossy_broadcast, watch_chan};
use anyhow::Context;
use hyprland::data::*;
use hyprland::shared::{HyprData, HyprDataVec};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio::task::JoinSet;
use tokio_stream::StreamExt;

// TODO: Detailed state (for menu), including clients
// TODO: channel to receive full refetch request
pub fn connect(reload_rx: ReloadRx) -> WatchRx<BasicDesktopState> {
    type HyprEvent = hyprland::event_listener::Event;
    let ev_tx = broadcast::Sender::new(100);

    let ev_rx = lossy_broadcast(ev_tx.subscribe());
    let (ws_tx, ws_rx) = watch_chan(BasicDesktopState::default());
    tokio::spawn(async move {
        tokio::pin!(ev_rx);

        let mut workspaces = Default::default();
        let mut monitors = HashMap::new();

        // TODO: Test if this is correct. If there is no event that needs to update both,
        // divide them into two seperate tasks.
        let fetch_rx = ev_rx
            .map(|ev| match ev {
                HyprEvent::MonitorAdded(_) => (false, true),
                HyprEvent::ActiveMonitorChanged(_) => (false, true),
                HyprEvent::MonitorRemoved(_) => (false, true),
                HyprEvent::WorkspaceChanged(_) => (false, true),

                HyprEvent::WorkspaceMoved(_) => (true, false),
                HyprEvent::WorkspaceAdded(_) => (true, false),
                HyprEvent::WorkspaceDeleted(_) => (true, false),
                HyprEvent::WorkspaceRenamed(_) => (true, false),

                _ => (false, false),
            })
            .merge(reload_rx.into_stream().map(|_| (true, true)))
            .filter(|&(f1, f2)| f1 || f2);

        tokio::pin!(fetch_rx);
        while let Some((upd_wss, upd_mons)) = fetch_rx.next().await {
            futures::future::join(
                async {
                    if upd_wss {
                        let Ok(wss) = Workspaces::get_async()
                            .await
                            .map_err(|err| log::error!("Failed to fetch workspaces: {err}"))
                        else {
                            return;
                        };
                        workspaces = wss.to_vec();
                    }
                },
                async {
                    if upd_mons {
                        let Ok(mrs) = Monitors::get_async()
                            .await
                            .map_err(|err| log::error!("Failed to fetch monitors: {err}"))
                        else {
                            return;
                        };
                        for Monitor {
                            id,
                            name,
                            active_workspace,
                            ..
                        } in mrs
                        {
                            monitors
                                .entry(id)
                                .and_modify(|(mname, mactive)| {
                                    *mactive = active_workspace.id;
                                    if mname as &str != name.as_str() {
                                        *mname = Arc::<str>::from(name.as_str());
                                    }
                                })
                                .or_insert_with(|| (name.into(), active_workspace.id));
                        }
                    }
                },
            )
            .await;

            let mut workspaces: Vec<_> = workspaces
                .iter()
                .map(
                    |Workspace {
                         id,
                         name,
                         monitor_id,
                         ..
                     }| {
                        let mon = monitor_id.and_then(|id| monitors.get(&id));
                        BasicWorkspace {
                            id: id.to_string().into(),
                            name: name.as_str().into(),
                            monitor: mon.as_ref().map(|(name, _)| name.clone()),
                            is_active: mon.is_some_and(|(_, active_id)| active_id == id),
                        }
                    },
                )
                .collect();
            workspaces.sort_unstable_by(|w1, w2| w1.name.cmp(&w2.name));
            if ws_tx.send(BasicDesktopState { workspaces }).is_err() {
                break;
            }
        }
    });

    tokio::spawn(async move {
        let mut hypr_events = hyprland::event_listener::EventStream::new();
        while let Some(event) = hypr_events.next().await {
            match event {
                Ok(ev) => {
                    if ev_tx.send(ev).is_err() {
                        log::warn!("Hyprland event channel closed");
                        return;
                    }
                }
                Err(err) => log::error!("Error with hypr event: {err}"),
            }
        }
        log::warn!("Hyprland event stream closed");
    });

    ws_rx
}

pub struct Hypr;
impl Module for Hypr {
    // FIXME: Refactor
    async fn run_instance(
        &self,
        ModuleArgs {
            mut act_tx,
            mut upd_rx,
            reload_rx,
            ..
        }: ModuleArgs,
        _cancel: crate::utils::CancelDropGuard,
    ) {
        let mut join = JoinSet::new();

        type HyprEvent = hyprland::event_listener::Event;

        let (ws_tx, mut ws_rx) = watch_chan(BasicDesktopState::default());
        join.spawn(async move {
            let ev_rx = hyprland::event_listener::EventStream::new()
                .filter_map(|res| res.context("Hyprland error").ok_or_log());
            tokio::pin!(ev_rx);

            let mut workspaces = Default::default();
            let mut monitors = HashMap::new();

            // TODO: Test if this is correct. If there is no event that needs to update both,
            // divide them into two seperate tasks.
            let fetch_rx = ev_rx
                .map(|ev| match ev {
                    HyprEvent::MonitorAdded(_) => (false, true),
                    HyprEvent::ActiveMonitorChanged(_) => (false, true),
                    HyprEvent::MonitorRemoved(_) => (false, true),
                    HyprEvent::WorkspaceChanged(_) => (false, true),

                    HyprEvent::WorkspaceMoved(_) => (true, false),
                    HyprEvent::WorkspaceAdded(_) => (true, false),
                    HyprEvent::WorkspaceDeleted(_) => (true, false),
                    HyprEvent::WorkspaceRenamed(_) => (true, false),

                    _ => (false, false),
                })
                .merge(reload_rx.into_stream().map(|_| (true, true)))
                .filter(|&(f1, f2)| f1 || f2);

            tokio::pin!(fetch_rx);
            while let Some((upd_wss, upd_mons)) = fetch_rx.next().await {
                futures::future::join(
                    async {
                        if upd_wss {
                            let Ok(wss) = Workspaces::get_async()
                                .await
                                .map_err(|err| log::error!("Failed to fetch workspaces: {err}"))
                            else {
                                return;
                            };
                            workspaces = wss.to_vec();
                        }
                    },
                    async {
                        if upd_mons {
                            let Ok(mrs) = Monitors::get_async()
                                .await
                                .map_err(|err| log::error!("Failed to fetch monitors: {err}"))
                            else {
                                return;
                            };
                            for Monitor {
                                id,
                                name,
                                active_workspace,
                                ..
                            } in mrs
                            {
                                monitors
                                    .entry(id)
                                    .and_modify(|(mname, mactive)| {
                                        *mactive = active_workspace.id;
                                        if mname as &str != name.as_str() {
                                            *mname = Arc::<str>::from(name.as_str());
                                        }
                                    })
                                    .or_insert_with(|| (name.into(), active_workspace.id));
                            }
                        }
                    },
                )
                .await;

                let mut workspaces: Vec<_> = workspaces
                    .iter()
                    .map(
                        |Workspace {
                             id,
                             name,
                             monitor_id,
                             ..
                         }| {
                            let mon = monitor_id.and_then(|id| monitors.get(&id));
                            BasicWorkspace {
                                id: id.to_string().into(),
                                name: name.as_str().into(),
                                monitor: mon.as_ref().map(|(name, _)| name.clone()),
                                is_active: mon.is_some_and(|(_, active_id)| active_id == id),
                            }
                        },
                    )
                    .collect();
                workspaces.sort_unstable_by(|w1, w2| w1.name.cmp(&w2.name));
                if ws_tx.send(BasicDesktopState { workspaces }).is_err() {
                    break;
                }
            }
        });

        join.spawn(async move {
            enum Upd<'a> {
                State(&'a BasicDesktopState),
                Update(ModuleUpd),
            }
            loop {
                let upd = tokio::select! {
                    res = ws_rx.changed() => match res {
                        Ok(()) => Upd::State(&ws_rx.borrow_and_update()),
                        Err(_) => break,
                    },
                    Some(upd) = upd_rx.next() => Upd::Update(upd),
                };
                match upd {
                    Upd::State(state) => {
                        let mut by_monitor = HashMap::new();
                        for ws in state.workspaces.iter() {
                            let Some(monitor) = ws.monitor.clone() else {
                                continue;
                            };
                            let wss = by_monitor.entry(monitor).or_insert_with(Vec::new);
                            wss.push(tui::StackItem::auto(tui::InteractElem::new(
                                Arc::new(ws.id.clone()),
                                tui::Text::plain(&ws.name).styled(tui::Style {
                                    fg: ws.is_active.then_some(tui::Color::Green),
                                    ..Default::default()
                                }),
                            )));
                            wss.push(tui::StackItem::spacing(1));
                        }
                        let by_monitor = by_monitor
                            .into_iter()
                            .map(|(k, v)| (k, tui::Stack::horizontal(v).into()))
                            .collect();

                        act_tx.emit(ModuleAct::RenderByMonitor(by_monitor));
                    }
                    Upd::Update(upd) => match upd {
                        ModuleUpd::Interact(ModuleInteract {
                            payload: ModuleInteractPayload { tag, .. },
                            kind: tui::InteractKind::Click(tui::MouseButton::Left),
                            ..
                        }) => {
                            // TODO: Switch ws
                        }
                        ModuleUpd::Interact(_) => {}
                    },
                }
            }
        });

        if let Some(res) = join.join_next().await {
            res.context("Hyprland module failed").ok_or_log();
        }
    }
}

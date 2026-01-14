use crate::data::{BasicDesktopState, BasicWorkspace};
use crate::modules::prelude::*;
use crate::tui;
use crate::utils::{
    Emit, LazyTaskHandle, ReloadRx, ReloadTx, SharedEmit, WatchRx, WatchTx, watch_chan,
};
use crate::utils::{LazyTask, ResultExt};
use anyhow::Context;
use hyprland::data::*;
use hyprland::shared::{HyprData, HyprDataVec};
use std::collections::HashMap;
use std::sync::Arc;
use tokio_stream::StreamExt;

// TODO: Detailed state (for menu), including clients

type HyprBackend = (WatchRx<BasicDesktopState>, ReloadTx);
pub struct HyprModule {
    updater: LazyTask<HyprBackend>,
}

// reqs:
// - Instantiation does nothing.
// - There is no leaked task if non-running instance is leaked
// - Updater task is shared between all instances
// - Once all instances stop running, updater task does too
// - If all instances go out of scope, then no resources are used. Including Arcs
//
// Logical consequeces
// - We need to do module tracking in the controller. We can drop unused modules though.
// - We want a way to create an updater task in `run_instance` such that reentering does not spawn
//   a new one. We can use a tokio mutex for this without any downsides because `run_instance`
//   already needs to run in its own task. the updater task can take an `Arc<Self>` because it will
//   be controlled through handles.

impl HyprModule {
    pub fn new() -> Self {
        Self {
            updater: LazyTask::new(),
        }
    }

    async fn enter_backend(&self) -> LazyTaskHandle<HyprBackend> {
        self.updater
            .enter(async || {
                let (tx, rx) = watch_chan(BasicDesktopState::default());
                let reload_tx = ReloadTx::new();
                (
                    Self::run_backend(tx, reload_tx.subscribe()),
                    (rx, reload_tx),
                )
            })
            .await
    }

    async fn run_backend(
        mut basic_tx: impl SharedEmit<BasicDesktopState>,
        mut reload_rx: ReloadRx,
    ) {
        let ev_rx = hyprland::event_listener::EventStream::new()
            .filter_map(|res| res.context("Hyprland error").ok_or_log());
        tokio::pin!(ev_rx);

        let mut workspaces = Default::default();
        let mut monitors = HashMap::new();

        loop {
            type HyprEvent = hyprland::event_listener::Event;
            let (upd_wss, upd_mons) = tokio::select! {
                () = reload_rx.wait() => (true, true),
                Some(ev) = ev_rx.next() => match ev {
                    HyprEvent::MonitorAdded(_) => (false, true),
                    HyprEvent::ActiveMonitorChanged(_) => (false, true),
                    HyprEvent::MonitorRemoved(_) => (false, true),
                    HyprEvent::WorkspaceChanged(_) => (false, true),

                    HyprEvent::WorkspaceMoved(_) => (true, false),
                    HyprEvent::WorkspaceAdded(_) => (true, false),
                    HyprEvent::WorkspaceDeleted(_) => (true, false),
                    HyprEvent::WorkspaceRenamed(_) => (true, false),

                    _ => continue,
                },
            };

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
            basic_tx.emit(BasicDesktopState { workspaces });
        }
    }
}
impl Module for HyprModule {
    async fn run_module_instance(
        self: Arc<Self>,
        ModuleArgs {
            mut act_tx,
            mut upd_rx,
            mut reload_rx,
            ..
        }: ModuleArgs,
        _cancel: crate::utils::CancelDropGuard,
    ) {
        let updater_handle = self.enter_backend().await;
        let (mut basic_rx, mut reload_tx) = updater_handle.backend().clone();

        enum Upd {
            State,
            Update(ModuleUpd),
        }
        loop {
            let upd = tokio::select! {
                res = basic_rx.changed() => match res {
                    Ok(()) => Upd::State,
                    Err(_) => break,
                },
                () = reload_rx.wait() => {
                    reload_tx.reload();
                    continue;
                }
                Some(upd) = upd_rx.next() => Upd::Update(upd),
            };
            match upd {
                Upd::State => {
                    let mut by_monitor = HashMap::new();
                    {
                        let state = basic_rx.borrow_and_update();
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
    }
}

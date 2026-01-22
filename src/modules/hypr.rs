use crate::data::{BasicDesktopState, BasicWorkspace};
use crate::modules::prelude::*;
use crate::tui;
use crate::utils::ResultExt;
use crate::utils::{Emit, ReloadRx, ReloadTx, SharedEmit, WatchRx, watch_chan};
use anyhow::Context;
use hyprland::data::*;
use hyprland::shared::{HyprData, HyprDataVec};
use std::collections::HashMap;
use std::sync::Arc;
use tokio_stream::StreamExt;
use tokio_util::task::AbortOnDropHandle;

// TODO: Detailed state (for menu), including clients

pub struct HyprModule {
    basic_rx: WatchRx<BasicDesktopState>,
    reload_tx: ReloadTx,
    _background: AbortOnDropHandle<()>,
}

impl HyprModule {
    async fn run_bg(basic_tx: impl SharedEmit<BasicDesktopState>, mut reload_rx: ReloadRx) {
        let ev_rx = hyprland::event_listener::EventStream::new()
            .filter_map(|res| res.context("Hyprland error").ok_or_log());
        tokio::pin!(ev_rx);

        let mut workspaces = Default::default();
        let mut monitors = HashMap::new();

        loop {
            type HyprEvent = hyprland::event_listener::Event;
            let (upd_wss, upd_mons) = tokio::select! {
                Some(()) = reload_rx.wait() => (true, true),
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
    type Config = ();

    fn connect() -> Self {
        let (basic_tx, basic_rx) = watch_chan(BasicDesktopState::default());
        let reload_tx = ReloadTx::new();
        Self {
            _background: AbortOnDropHandle::new(tokio::spawn(Self::run_bg(
                basic_tx,
                reload_tx.subscribe(),
            ))),
            basic_rx,
            reload_tx,
        }
    }

    async fn run_module_instance(
        self: Arc<Self>,
        cfg: Self::Config,
        ModuleArgs {
            act_tx,
            mut reload_rx,
            ..
        }: ModuleArgs,
        _cancel: crate::utils::CancelDropGuard,
    ) {
        let mut basic_rx = self.basic_rx.clone();
        let mut reload_tx = self.reload_tx.clone();

        loop {
            tokio::select! {
                res = basic_rx.changed() => match res {
                    Ok(()) => {},
                    Err(_) => break,
                },
                Some(()) = reload_rx.wait() => {
                    reload_tx.reload();
                    continue;
                }
            };
            let mut by_monitor = HashMap::new();
            for ws in basic_rx.borrow_and_update().workspaces.iter() {
                let Some(monitor) = ws.monitor.clone() else {
                    continue;
                };
                let wss = by_monitor.entry(monitor).or_insert_with(Vec::new);
                wss.push(tui::StackItem::auto(
                    tui::Elem::from(tui::RawPrint::plain(&ws.name).styled(tui::Style {
                        fg: ws.is_active.then_some(tui::Color::Green),
                        ..Default::default()
                    }))
                    .on_interact_fn(|interact| {
                        // TODO: Switch ws on lclick
                        // TODO: Show workspace info on rclick/hover
                    }),
                ));
                wss.push(tui::StackItem::spacing(1));
            }
            let by_monitor = by_monitor
                .into_iter()
                .map(|(k, v)| (k, tui::StackItem::auto(tui::Stack::horizontal(v))))
                .collect();

            act_tx.emit(ModuleAct::RenderByMonitor(by_monitor));
        }
    }
}

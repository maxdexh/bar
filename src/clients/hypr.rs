use crate::data::{BasicDesktopState, BasicWorkspace, WorkspaceId};
use crate::utils::{ReloadRx, WatchRx, watch_chan};
use crate::utils::{ResultExt, WatchTx};
use anyhow::Context;
use futures::StreamExt;
use hyprland::data::*;
use hyprland::shared::{HyprData, HyprDataVec};
use std::collections::HashMap;
use std::sync::Arc;
use tokio_util::task::AbortOnDropHandle;

// TODO: Detailed state (for menu), including clients

pub struct HyprClient {
    pub basic_rx: WatchRx<BasicDesktopState>,
    _background: AbortOnDropHandle<()>,
}
impl HyprClient {
    pub fn switch_workspace(&self, id: WorkspaceId) {
        tokio::spawn(async move {
            hyprland::dispatch::Dispatch::call_async(hyprland::dispatch::DispatchType::Workspace(
                // HACK: workspace ids do not work correctly, so we use the name instead
                // (see creation of BasicWorkspace)
                hyprland::dispatch::WorkspaceIdentifierWithSpecial::Name(id.as_str()),
            ))
            .await
            .ok_or_log();
        });
    }
}

async fn run_bg(basic_tx: WatchTx<BasicDesktopState>, mut reload_rx: ReloadRx) {
    let ev_rx = hyprland::event_listener::EventStream::new()
        .filter_map(async |res| res.context("Hyprland error").ok_or_log());
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
                        // HACK: workspace ids do not work correctly, so we use the name instead
                        // (see switch_workspace)
                        id: name.as_str().into(),
                        name: name.as_str().into(),
                        monitor: mon.as_ref().map(|(name, _)| name.clone()),
                        is_active: mon.is_some_and(|(_, active_id)| active_id == id),
                    }
                },
            )
            .collect();

        workspaces.sort_unstable_by(|w1, w2| w1.name.cmp(&w2.name));
        basic_tx.send_replace(BasicDesktopState { workspaces });
    }
}

pub fn connect(reload_rx: ReloadRx) -> HyprClient {
    let (basic_tx, basic_rx) = watch_chan(BasicDesktopState::default());
    HyprClient {
        _background: AbortOnDropHandle::new(tokio::spawn(run_bg(basic_tx, reload_rx))),
        basic_rx,
    }
}

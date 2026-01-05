use crate::data::{BasicDesktopState, BasicWorkspace};
use crate::utils::{Emit, ReloadRx, lossy_broadcast};
use futures::Stream;
use hyprland::data::*;
use hyprland::shared::{HyprData, HyprDataVec};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio_stream::StreamExt;

// TODO: Detailed state (for menu), including clients
// TODO: channel to receive full refetch request
pub fn connect(reload_rx: ReloadRx) -> impl Stream<Item = BasicDesktopState> {
    type HyprEvent = hyprland::event_listener::Event;
    let ev_tx = broadcast::Sender::new(100);

    let ev_rx = lossy_broadcast(ev_tx.subscribe());
    let (mut ws_tx, ws_rx) = broadcast::channel(10);
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
            if ws_tx.emit(BasicDesktopState { workspaces }).is_break() {
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

    lossy_broadcast(ws_rx)
}

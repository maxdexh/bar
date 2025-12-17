use crate::data::{ActiveMonitorInfo, BasicDesktopState, BasicMonitor, BasicWorkspace};
use crate::utils::{ResultExt as _, fused_lossy_stream};
use futures::Stream;
use hyprland::data::*;
use hyprland::shared::HyprData;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio_stream::StreamExt;

// TODO: Detailed state (for menu), including clients
// TODO: channel to receive full refetch request
pub fn subscribe() -> (
    impl Stream<Item = BasicDesktopState>,
    impl Stream<Item = Option<ActiveMonitorInfo>>,
) {
    type HyprEvent = hyprland::event_listener::Event;
    let ev_tx = broadcast::Sender::new(100);

    let events = fused_lossy_stream(ev_tx.subscribe());
    let (ws_tx, ws_rx) = broadcast::channel(10);
    let (am_tx, am_rx) = broadcast::channel(10);
    tokio::spawn(async move {
        tokio::pin!(events);

        let mut workspaces = Default::default();
        let mut monitors = Default::default();
        let mut active_monitor = Default::default();

        let update_wss = async |workspaces: &mut _| {
            if let Some(wss) = Workspaces::get_async()
                .await
                .ok_or_log("Failed to fetch workspaces")
            {
                *workspaces = wss
                    .into_iter()
                    .map(
                        |Workspace {
                             id,
                             name,
                             monitor_id,
                             ..
                         }| BasicWorkspace {
                            id: id.to_string().into(),
                            name: name.into(),
                            monitor: monitor_id.map(|id| id.to_string().into()),
                        },
                    )
                    .collect();

                // This should always succeed
                if let Some(slice) = Arc::get_mut(workspaces) {
                    slice.sort_by(|w1, w2| w1.name.cmp(&w2.name));
                }
            }
        };
        update_wss(&mut workspaces).await;

        let update_mons = async |monitors: &mut _, active_monitor: &mut _| {
            if let Some(mrs) = Monitors::get_async()
                .await
                .ok_or_log("Failed to fetch monitors")
            {
                let active = mrs.iter().find(|mon| mon.focused);
                *active_monitor = active.map(|mon| mon.id.to_string().into());
                am_tx
                    .send(active.map(
                        |&Monitor {
                             id,
                             width,
                             height,
                             scale,
                             ref name,
                             ..
                         }| ActiveMonitorInfo {
                            width: width.into(),
                            height: height.into(),
                            scale: scale.into(),
                            name: name.as_str().into(),
                            id: id.to_string().into(),
                        },
                    ))
                    .ok_or_log("Failed to send active monitor information");
                *monitors = mrs
                    .into_iter()
                    .map(|mr| BasicMonitor {
                        id: mr.id.to_string().into(),
                        active_workspace: mr.active_workspace.id.to_string().into(),
                        name: mr.name.into(),
                    })
                    .collect();
            }
        };
        update_mons(&mut monitors, &mut active_monitor).await;

        let send_update = |workspaces: &_, monitors: &_, active_monitor: &_| {
            if let Err(err) = ws_tx.send(BasicDesktopState {
                workspaces: Arc::clone(workspaces),
                monitors: Arc::clone(monitors),
                active_monitor: Option::clone(active_monitor),
            }) {
                log::warn!("Hyprland state channel closed: {err}");
            }
        };
        send_update(&workspaces, &monitors, &active_monitor);

        while let Some(ev) = events.next().await {
            let (upd_wss, upd_mons) = match ev {
                HyprEvent::WorkspaceMoved(_) => (true, true),
                HyprEvent::MonitorAdded(_) => (false, true),
                HyprEvent::MonitorRemoved(_) => (true, true),
                HyprEvent::ActiveMonitorChanged(_) => (false, true),

                // TODO: More filtering
                _ => (true, false),
            };

            if !upd_wss && !upd_mons {
                continue;
            }

            futures::future::join(
                futures::future::OptionFuture::from(upd_wss.then(|| update_wss(&mut workspaces))),
                futures::future::OptionFuture::from(
                    upd_mons.then(|| update_mons(&mut monitors, &mut active_monitor)),
                ),
            )
            .await;
            send_update(&workspaces, &monitors, &active_monitor);
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
                // TODO: Do we need to handle UnexpectedEof by exiting?
                Err(err) => log::error!("Error with hypr event: {err}"),
            }
        }
        log::warn!("Hyprland event stream closed");
    });

    (fused_lossy_stream(ws_rx), fused_lossy_stream(am_rx))
}

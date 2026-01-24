use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::Context;
use futures::StreamExt as _;
use futures::{FutureExt, Stream};
use system_tray::item::StatusNotifierItem;
use tokio::task::JoinSet;
use tokio_util::task::AbortOnDropHandle;

use crate::utils::WatchTx;
use crate::{
    tui,
    utils::{ReloadRx, ResultExt, UnbTx, WatchRx, unb_chan, watch_chan},
};

#[derive(Debug, Clone)]
pub struct TrayMenuInteract {
    pub addr: Arc<str>,
    pub menu_path: Arc<str>,
    pub id: i32,
    pub kind: tui::InteractKind,
}

#[derive(Debug, Default)]
pub struct TrayState {
    pub items: HashMap<Arc<str>, StatusNotifierItem>,
    pub menus: HashMap<Arc<str>, TrayMenuExt>,
}

#[derive(serde::Deserialize, serde::Serialize, Debug, Clone)]
pub struct TrayMenuExt {
    pub id: u32,
    pub menu_path: Option<Arc<str>>,
    pub submenus: Vec<system_tray::menu::MenuItem>,
}

pub struct TrayClient {
    pub state_rx: WatchRx<TrayState>,
    pub menu_interact_tx: UnbTx<TrayMenuInteract>,
    _background: AbortOnDropHandle<()>,
}
pub fn connect(reload_rx: ReloadRx) -> TrayClient {
    let (state_tx, state_rx) = watch_chan(Default::default());
    let (menu_interact_tx, menu_interact_rx) = unb_chan();
    TrayClient {
        _background: AbortOnDropHandle::new(tokio::spawn(run_bg(
            state_tx,
            menu_interact_rx,
            reload_rx,
        ))),
        state_rx,
        menu_interact_tx,
    }
}
async fn run_bg(
    state_tx: WatchTx<TrayState>,
    menu_interact_rx: impl Stream<Item = TrayMenuInteract> + 'static + Send,
    mut reload_rx: ReloadRx,
) {
    let client = loop {
        match system_tray::client::Client::new().await {
            Ok(it) => break it,
            res @ Err(_) => {
                res.context("Failed to connect to system tray").ok_or_log();

                tokio::select! {
                    () = tokio::time::sleep(Duration::from_secs(90)) => (),
                    Some(()) = reload_rx.wait() => (),
                }
            }
        }
    };

    let mut tasks = JoinSet::<()>::new();

    {
        let (tx, rx) = unb_chan();

        // Minimize lagged events
        let mut event_rx = client.subscribe();
        tasks.spawn(async move {
            while match event_rx.recv().await {
                Ok(ev) => tx.send(ev).is_ok(),
                Err(tokio::sync::broadcast::error::RecvError::Closed) => false,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    log::warn!("Lagged {n} events from system-tray client");
                    true
                }
            } {}
        });

        tasks.spawn(run_state_fetcher(client.items(), rx, state_tx, reload_rx));
    }
    tasks.spawn(run_menu_interaction(client, menu_interact_rx));

    if let Some(res @ Err(_)) = tasks.join_next().await {
        res.context("Systray module failed").ok_or_log();
    }
}
async fn run_state_fetcher(
    state_mutex: Arc<std::sync::Mutex<system_tray::data::BaseMap>>,
    client_rx: impl Stream<Item = system_tray::client::Event>,
    state_tx: WatchTx<TrayState>,
    mut reload_rx: ReloadRx,
) {
    let fetch_blocking = move || {
        let lock = state_mutex.lock().unwrap_or_else(|it| it.into_inner());

        let mut items = HashMap::with_capacity(lock.len());
        let mut menus = HashMap::with_capacity(lock.len());

        for (addr, (item, menu)) in &*lock {
            let addr = Arc::<str>::from(addr.as_str());
            if let Some(menu) = &menu {
                menus.insert(
                    addr.clone(),
                    TrayMenuExt {
                        id: menu.id,
                        menu_path: None,
                        submenus: menu.submenus.clone(),
                    },
                );
            }
            items.insert(addr, item.clone());
        }
        (items, menus)
    };
    let fetch = || async {
        tokio::task::spawn_blocking(fetch_blocking.clone())
            .await
            .ok_or_log()
    };

    let mut menu_paths = HashMap::new();

    tokio::pin!(client_rx);
    loop {
        let mut ev_opt = tokio::select! {
            ev = client_rx.next() => {
                if ev.is_none() {
                    log::warn!("Systray client disconnected");
                    break
                }
                ev
            }
            Some(()) = reload_rx.wait() => None,
        };
        while let Some(ev) = ev_opt {
            if let system_tray::client::Event::Update(
                addr,
                system_tray::client::UpdateEvent::MenuConnect(menu_path),
            ) = ev
            {
                log::trace!("Connected menu {menu_path} for addr {addr}");
                menu_paths.insert(addr, Arc::<str>::from(menu_path));
            }
            ev_opt = client_rx.next().now_or_never().flatten();
        }

        let Some((items, mut menus)) = fetch().await else {
            continue;
        };
        menu_paths.retain(|addr, menu_path| {
            if let Some(menu) = menus.get_mut(addr as &str) {
                menu.menu_path = Some(menu_path.clone());
                true
            } else {
                false
            }
        });
        state_tx.send_replace(TrayState { items, menus });
    }
}
async fn run_menu_interaction(
    client: system_tray::client::Client,
    interact_rx: impl Stream<Item = TrayMenuInteract>,
) {
    tokio::pin!(interact_rx);
    while let Some(interact) = interact_rx.next().await {
        let TrayMenuInteract {
            addr,
            menu_path,
            id,
            kind,
        } = interact;

        match kind {
            tui::InteractKind::Click(tui::MouseButton::Left) => {
                client
                    .activate(system_tray::client::ActivateRequest::MenuItem {
                        address: str::to_owned(&addr),
                        menu_path: str::to_owned(&menu_path),
                        submenu_id: id,
                    })
                    .await
                    .context("Failed to send ActivateRequest")
                    .ok_or_log();
            }
            tui::InteractKind::Hover => {
                // TODO: Highlight
            }
            _ => (),
        }
    }
    log::warn!("Tray interact stream was closed");
}

use std::{collections::HashMap, sync::Arc};

use anyhow::Context;
use futures::{FutureExt, Stream};
use system_tray::item::StatusNotifierItem;
use tokio::sync::broadcast;
use tokio_stream::StreamExt as _;

use crate::utils::{ReloadRx, ResultExt, lossy_broadcast};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct TrayMenuInteract {
    pub addr: Arc<str>,
    pub menu_path: Arc<str>,
    pub id: i32,
}

type TrayEvent = system_tray::client::Event;
#[derive(Debug, serde::Serialize, serde::Deserialize, Default)]
pub struct TrayState {
    pub items: Arc<[(Arc<str>, StatusNotifierItem)]>,
    pub menus: Arc<[(Arc<str>, TrayMenuExt)]>,
}

pub fn connect(
    reload_rx: ReloadRx,
) -> (
    broadcast::Sender<TrayMenuInteract>,
    impl Stream<Item = TrayState>,
) {
    let (interact_tx, interact_rx) = broadcast::channel(50);

    let deferred_stream = tokio::spawn(system_tray::client::Client::new()).map(|res| {
        match res
            .context("Failed to spawn task for system tray client")
            .ok_or_log()
            .transpose()
            .context("Failed to connect to system tray")
            .ok_or_log()
            .flatten()
        {
            Some(client) => {
                let stream = mk_stream(client.items(), client.subscribe(), reload_rx);
                tokio::spawn(run_interaction(client, interact_rx));
                stream
            }
            None => mk_stream(Default::default(), broadcast::channel(1).1, reload_rx),
        }
    });
    (
        interact_tx,
        // TODO: is there a better way of turning impl Future<Output: Stream> into a stream?
        futures::StreamExt::flatten(deferred_stream.into_stream()),
    )
}

async fn run_interaction(
    client: system_tray::client::Client,
    interact_rx: broadcast::Receiver<TrayMenuInteract>,
) {
    let interacts = lossy_broadcast(interact_rx);
    tokio::pin!(interacts);
    while let Some(interact) = interacts.next().await {
        let TrayMenuInteract {
            addr,
            menu_path,
            id,
        } = interact;

        if let Err(err) = client
            .activate(system_tray::client::ActivateRequest::MenuItem {
                address: str::to_owned(&addr),
                menu_path: str::to_owned(&menu_path),
                submenu_id: id,
            })
            .await
        {
            log::error!("Failed to send ActivateRequest: {err}");
        }
    }
    log::warn!("Tray interact stream was closed");
}

fn mk_stream(
    items_mutex: Arc<std::sync::Mutex<system_tray::data::BaseMap>>,
    client_rx: broadcast::Receiver<TrayEvent>,
    reload_rx: ReloadRx,
) -> impl Stream<Item = TrayState> {
    let menu_paths = Arc::new(std::sync::Mutex::new(HashMap::new()));

    let handle_event = move |ev| {
        let items_mutex = items_mutex.clone();
        let menu_paths = menu_paths.clone();

        tokio::task::spawn_blocking(move || {
            let mut menu_paths = menu_paths.lock().unwrap();
            let items_lock = items_mutex.lock().unwrap();

            if let Some(system_tray::client::Event::Update(
                addr,
                system_tray::client::UpdateEvent::MenuConnect(menu_path),
            )) = &ev
            {
                log::trace!("Connected menu {menu_path} for addr {addr}");
                menu_paths.insert(
                    Arc::<str>::from(addr.as_str()),
                    Arc::<str>::from(menu_path.as_str()),
                );
            }
            let mut items = Vec::with_capacity(items_lock.len());
            let mut menus = Vec::with_capacity(items_lock.len());
            for (addr, (item, menu)) in &*items_lock {
                let addr = Arc::<str>::from(addr.as_str());
                if let Some(&system_tray::menu::TrayMenu { id, ref submenus }) = menu.as_ref() {
                    menus.push((
                        addr.clone(),
                        TrayMenuExt {
                            id,
                            menu_path: menu_paths.get(&addr as &str).cloned(),
                            submenus: submenus.clone(),
                        },
                    ));
                }
                items.push((addr, item.clone()));
            }
            TrayState {
                items: items.into(),
                menus: menus.into(),
            }
        })
    };

    lossy_broadcast(client_rx)
        .map(Some)
        .merge(reload_rx.into_stream().map(|()| None))
        .then(handle_event)
        .filter_map(|res| {
            res.map_err(|err| log::error!("Failed to join blocking task: {err}"))
                .ok()
        })
}

#[derive(serde::Deserialize, serde::Serialize, Debug, Clone)]
pub struct TrayMenuExt {
    pub id: u32,
    pub menu_path: Option<Arc<str>>,
    pub submenus: Vec<system_tray::menu::MenuItem>,
}

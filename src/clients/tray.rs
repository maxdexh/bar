use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

use futures::{FutureExt, Stream};
use system_tray::item::StatusNotifierItem;
use tokio::sync::{broadcast, mpsc};
use tokio_stream::StreamExt as _;

use crate::utils::ResultExt as _;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct TrayMenuInteract {
    pub addr: Arc<str>,
    pub menu_path: Arc<str>,
    pub id: i32,
}

type TrayEvent = system_tray::client::Event;
#[derive(Debug, serde::Serialize, serde::Deserialize, Default)]
pub struct TrayState {
    // FIXME: Use Vec (or Arc<[T]>)
    pub items: BTreeMap<Arc<str>, StatusNotifierItem>,
    pub menus: HashMap<Arc<str>, TrayMenuExt>,
}

pub fn connect() -> (
    mpsc::Sender<TrayMenuInteract>,
    impl Stream<Item = (TrayEvent, TrayState)>,
) {
    let (interact_tx, interact_rx) = mpsc::channel(50);

    let deferred_stream = tokio::spawn(system_tray::client::Client::new()).map(|res| {
        res.ok_or_log("Failed to spawn task for system tray client")
            .transpose()
            .ok_or_log("Failed to connect to system tray")
            .flatten()
            .map_or_else(
                || mk_stream(Default::default(), broadcast::channel(1).1),
                |client| {
                    let stream = mk_stream(client.items(), client.subscribe());
                    tokio::spawn(run_interaction(client, interact_rx));
                    stream
                },
            )
    });
    (
        interact_tx,
        // TODO: is there a better way of turning impl Future<Output: Stream> into a stream?
        futures::StreamExt::flatten(deferred_stream.into_stream()),
    )
}

async fn run_interaction(
    client: system_tray::client::Client,
    interact_rx: mpsc::Receiver<TrayMenuInteract>,
) {
    let mut interacts = tokio_stream::wrappers::ReceiverStream::new(interact_rx);
    while let Some(interact) = interacts.next().await {
        let TrayMenuInteract {
            addr,
            menu_path,
            id,
        } = interact;

        client
            .activate(system_tray::client::ActivateRequest::MenuItem {
                address: str::to_owned(&addr),
                menu_path: str::to_owned(&menu_path),
                submenu_id: id,
            })
            .await
            .ok_or_log("Failed to send ActivateRequest");
    }
    log::warn!("Tray interact stream was closed");
}

fn mk_stream(
    items_mutex: Arc<std::sync::Mutex<system_tray::data::BaseMap>>,
    client_rx: broadcast::Receiver<TrayEvent>,
) -> impl Stream<Item = (TrayEvent, TrayState)> {
    let menu_paths = Arc::new(std::sync::Mutex::new(HashMap::new()));
    tokio_stream::wrappers::BroadcastStream::new(client_rx)
        .filter_map(|res| res.ok_or_log("Failed to receive tray event"))
        .then(move |ev| {
            let items_mutex = items_mutex.clone();
            let menu_paths = menu_paths.clone();
            tokio::task::spawn_blocking(move || {
                let mut menu_paths = menu_paths.lock().unwrap();
                let items_lock = items_mutex.lock().unwrap();

                // TODO: remove too
                if let system_tray::client::Event::Update(
                    addr,
                    system_tray::client::UpdateEvent::MenuConnect(menu_path),
                ) = &ev
                {
                    log::trace!("Connected menu {menu_path} for addr {addr}");
                    menu_paths.insert(
                        Arc::<str>::from(addr.as_str()),
                        Arc::<str>::from(menu_path.as_str()),
                    );
                }
                let mut items = BTreeMap::new();
                let mut menus = HashMap::new();
                for (addr, (item, menu)) in &*items_lock {
                    let addr = Arc::<str>::from(addr.as_str());
                    if let Some(&system_tray::menu::TrayMenu { id, ref submenus }) = menu.as_ref() {
                        menus.insert(
                            addr.clone(),
                            TrayMenuExt {
                                id,
                                menu_path: menu_paths.get(&addr as &str).cloned(),
                                submenus: submenus.clone(),
                            },
                        );
                    }
                    items.insert(addr, item.clone());
                }
                (ev, TrayState { items, menus })
            })
        })
        .filter_map(|res| res.ok_or_log("Failed to join blocking task"))
}

#[derive(serde::Deserialize, serde::Serialize, Debug, Clone)]
pub struct TrayMenuExt {
    pub id: u32,
    pub menu_path: Option<Arc<str>>,
    pub submenus: Vec<system_tray::menu::MenuItem>,
}

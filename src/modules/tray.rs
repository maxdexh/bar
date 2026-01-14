use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::Context;
use futures::{FutureExt, Stream};
use system_tray::item::StatusNotifierItem;
use tokio::task::JoinSet;
use tokio_stream::StreamExt as _;

use crate::{
    modules::prelude::*,
    tui,
    utils::{
        Emit, LazyTask, LazyTaskHandle, ReloadRx, ReloadTx, ResultExt, SharedEmit, UnbTx, WatchRx,
        await_first, lossy_broadcast, unb_chan, watch_chan,
    },
};

#[derive(Debug)]
pub struct TrayMenuInteract {
    pub addr: Arc<str>,
    pub menu_path: Arc<str>,
    pub id: i32,
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

#[derive(Debug)]
struct TrayInteractTag {
    addr: Arc<str>,
}

// FIXME: Refactor
#[derive(Clone)]
struct TrayBackend {
    state_rx: WatchRx<TrayState>,
    menu_interact_tx: UnbTx<Arc<TrayMenuInteract>>,
    reload_tx: ReloadTx,
}
pub struct TrayModule {
    background: LazyTask<TrayBackend>,
}
impl TrayModule {
    pub fn new() -> Self {
        Self {
            background: LazyTask::new(),
        }
    }
    async fn enter_backend(&self) -> LazyTaskHandle<TrayBackend> {
        self.background
            .enter(|| async {
                let (state_tx, state_rx) = watch_chan(Default::default());
                let (menu_interact_tx, menu_interact_rx) = unb_chan();
                let reload_tx = ReloadTx::new();
                (
                    Self::run_backend(state_tx, menu_interact_rx, reload_tx.subscribe()),
                    TrayBackend {
                        state_rx,
                        menu_interact_tx,
                        reload_tx,
                    },
                )
            })
            .await
    }
    async fn run_backend(
        state_tx: impl SharedEmit<TrayState>,
        menu_interact_rx: impl Stream<Item = Arc<TrayMenuInteract>> + 'static + Send,
        mut reload_rx: ReloadRx,
    ) {
        let client = loop {
            match system_tray::client::Client::new().await {
                Ok(it) => break it,
                res @ Err(_) => {
                    res.context("Failed to connect to system tray").ok_or_log();

                    let sleep = tokio::time::sleep(Duration::from_secs(90));
                    let reload = reload_rx.wait();
                    await_first!(sleep, reload);
                }
            }
        };

        let mut tasks = JoinSet::<()>::new();

        {
            let (mut tx, rx) = unb_chan();

            // Minimize lagged events
            let event_rx = lossy_broadcast(client.subscribe());
            tasks.spawn(async move {
                tokio::pin!(event_rx);
                while let Some(ev) = event_rx.next().await {
                    tx.emit(ev);
                }
            });

            tasks.spawn(Self::run_state_fetcher(
                client.items(),
                rx,
                state_tx,
                reload_rx,
            ));
        }
        tasks.spawn(Self::run_menu_interaction(client, menu_interact_rx));

        if let Some(res @ Err(_)) = tasks.join_next().await {
            res.context("Systray module failed").ok_or_log();
        }
    }
    async fn run_state_fetcher(
        state_mutex: Arc<std::sync::Mutex<system_tray::data::BaseMap>>,
        client_rx: impl Stream<Item = system_tray::client::Event>,
        mut state_tx: impl SharedEmit<TrayState>,
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
                () = reload_rx.wait() => None,
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
            state_tx.emit(TrayState { items, menus })
        }
    }
    async fn run_menu_interaction(
        client: system_tray::client::Client,
        interact_rx: impl Stream<Item = Arc<TrayMenuInteract>>,
    ) {
        tokio::pin!(interact_rx);
        while let Some(interact) = interact_rx.next().await {
            let TrayMenuInteract {
                addr,
                menu_path,
                id,
            } = &*interact;

            if let Err(err) = client
                .activate(system_tray::client::ActivateRequest::MenuItem {
                    address: str::to_owned(addr),
                    menu_path: str::to_owned(menu_path),
                    submenu_id: *id,
                })
                .await
            {
                log::error!("Failed to send ActivateRequest: {err}");
            }
        }
        log::warn!("Tray interact stream was closed");
    }
}

impl Module for TrayModule {
    async fn run_module_instance(
        self: Arc<Self>,
        ModuleArgs {
            act_tx,
            mut upd_rx,
            mut reload_rx,
            ..
        }: ModuleArgs,
        _cancel: crate::utils::CancelDropGuard,
    ) {
        let handle = self.enter_backend().await;
        let TrayBackend {
            state_rx,
            menu_interact_tx: mut interact_tx,
            mut reload_tx,
        } = handle.backend().clone();

        let reload_fut = reload_tx.reload_on(&mut reload_rx);

        let bar_fut = async {
            let mut act_tx = act_tx.clone();
            let mut state_rx = state_rx.clone();
            while state_rx.changed().await.is_ok() {
                let items = state_rx.borrow_and_update().items.clone();
                let mut parts = Vec::new();
                for (addr, item) in items.iter() {
                    for system_tray::item::IconPixmap {
                        width,
                        height,
                        pixels,
                    } in item.icon_pixmap.as_deref().unwrap_or(&[])
                    {
                        let mut img = match image::RgbaImage::from_vec(
                            width.cast_unsigned(),
                            height.cast_unsigned(),
                            pixels.clone(),
                        ) {
                            Some(img) => img,
                            None => {
                                log::error!("Failed to load image from bytes");
                                continue;
                            }
                        };

                        // https://users.rust-lang.org/t/argb32-color-model/92061/4
                        for image::Rgba(pixel) in img.pixels_mut() {
                            *pixel = u32::from_be_bytes(*pixel).rotate_left(8).to_be_bytes();
                        }
                        let mut png_data = Vec::new();
                        if let Err(err) =
                            img.write_with_encoder(image::codecs::png::PngEncoder::new(
                                std::io::Cursor::new(&mut png_data),
                            ))
                        {
                            log::error!("Error encoding image: {err}");
                            continue;
                        }

                        parts.extend([
                            tui::StackItem::auto(tui::InteractElem::new(
                                Arc::new(TrayInteractTag { addr: addr.clone() }),
                                tui::Image::load_or_empty(png_data, image::ImageFormat::Png),
                            )),
                            tui::StackItem::spacing(1),
                        ])
                    }
                }
                let tui = tui::Stack::horizontal(parts);
                act_tx.emit(ModuleAct::RenderAll(tui.into()));
            }
        };

        let interact_fut = async {
            let mut act_tx = act_tx.clone();
            while let Some(upd) = upd_rx.next().await {
                match upd {
                    ModuleUpd::Interact(ModuleInteract {
                        location,
                        payload: ModuleInteractPayload { tag, monitor },
                        kind,
                    }) => {
                        if let Ok(menu_interact) = Arc::downcast(tag.clone()) {
                            interact_tx.emit(menu_interact);
                            continue;
                        }
                        let Some(TrayInteractTag { addr }) = tag.downcast_ref() else {
                            continue;
                        };

                        let menu_kind;
                        let tui;
                        match kind {
                            tui::InteractKind::Hover => {
                                let items = state_rx.borrow().items.clone();
                                let Some(system_tray::item::Tooltip {
                                    icon_name: _,
                                    icon_data: _,
                                    title,
                                    description,
                                }) = items.get(addr).and_then(|item| item.tool_tip.as_ref())
                                else {
                                    log::error!("Unknown tray addr {addr}");
                                    continue;
                                };

                                menu_kind = MenuKind::Tooltip;
                                tui = tui::Stack::vertical([
                                    tui::StackItem::auto(tui::Stack::horizontal([
                                        tui::StackItem::new(tui::Constr::Fill(1), tui::Elem::Empty),
                                        tui::StackItem::auto(
                                            tui::Text::plain(title.as_str()).styled(tui::Style {
                                                modifier: tui::Modifier {
                                                    bold: true,
                                                    ..Default::default()
                                                },
                                                ..Default::default()
                                            }),
                                        ),
                                        tui::StackItem::new(tui::Constr::Fill(1), tui::Elem::Empty),
                                    ])),
                                    tui::StackItem::auto(tui::Text::plain(description.as_str())),
                                ])
                                .into();
                            }
                            tui::InteractKind::Click(tui::MouseButton::Right) => {
                                let menus = state_rx.borrow().menus.clone();
                                let Some(TrayMenuExt {
                                    menu_path,
                                    submenus,
                                    ..
                                }) = menus.get(addr)
                                else {
                                    log::error!("Unknown tray addr {addr}");
                                    continue;
                                };

                                menu_kind = MenuKind::Context;
                                tui = tui::Block {
                                    borders: tui::Borders::all(),
                                    border_style: tui::Style {
                                        fg: Some(tui::Color::DarkGrey),
                                        ..Default::default()
                                    },
                                    border_set: tui::LineSet::thick(),
                                    inner: Some(Box::new(tray_menu_to_tui(
                                        0,
                                        submenus,
                                        addr,
                                        menu_path.as_ref(),
                                    ))),
                                }
                                .into();
                            }
                            _ => continue,
                        };
                        act_tx.emit(ModuleAct::OpenMenu(OpenMenu {
                            monitor,
                            tui,
                            pos: location,
                            menu_kind,
                        }));
                    }
                }
            }
        };

        await_first!(reload_fut, bar_fut, interact_fut);
    }
}
fn tray_menu_item_to_tui(
    depth: u16,
    item: &system_tray::menu::MenuItem,
    addr: &Arc<str>,
    menu_path: Option<&Arc<str>>,
) -> Option<tui::Elem> {
    use system_tray::menu::*;
    let main_elem = match item {
        MenuItem { visible: false, .. } => return None,
        MenuItem {
            visible: true,
            menu_type: MenuType::Separator,
            ..
        } => tui::Block {
            borders: tui::Borders {
                top: true,
                ..Default::default()
            },
            border_style: tui::Style {
                fg: Some(tui::Color::DarkGrey),
                ..Default::default()
            },
            border_set: tui::LineSet::normal(),
            inner: None,
        }
        .into(),

        MenuItem {
            id,
            menu_type: MenuType::Standard,
            label: Some(label),
            enabled: _,
            visible: true,
            icon_name: _,
            icon_data,
            shortcut: _,
            toggle_type: _, // TODO: implement toggle
            toggle_state: _,
            children_display: _,
            disposition: _, // TODO: what to do with this?
            submenu: _,
        } => {
            let elem = tui::Stack::horizontal([
                tui::StackItem::spacing(depth + 1),
                if let Some(icon) = icon_data {
                    let mut lines = label.lines();
                    let first_line = lines.next().unwrap_or_default();
                    tui::StackItem::auto(tui::Stack::vertical([
                        tui::StackItem::length(
                            1,
                            tui::Stack::horizontal([
                                tui::StackItem::auto(tui::Image::load_or_empty(
                                    icon,
                                    image::ImageFormat::Png,
                                )),
                                tui::StackItem::spacing(1),
                                tui::StackItem::auto(tui::Text::plain(first_line)),
                            ]),
                        ),
                        tui::StackItem::auto(tui::Text::plain(
                            lines.collect::<Vec<&str>>().join("\n"),
                        )),
                    ]))
                } else {
                    tui::StackItem::auto(tui::Text::plain(label))
                },
                tui::StackItem::spacing(1),
            ])
            .into();
            match menu_path {
                Some(it) => tui::InteractElem {
                    elem,
                    tag: Arc::new(TrayMenuInteract {
                        addr: addr.clone(),
                        menu_path: it.clone(),
                        id: *id,
                    }),
                }
                .into(),
                None => elem,
            }
        }

        _ => {
            log::error!("Unhandled menu item: {item:#?}");
            return None;
        }
    };

    Some(if item.submenu.is_empty() {
        main_elem
    } else {
        tui::Stack::vertical([
            tui::StackItem::auto(main_elem),
            tui::StackItem::auto(tray_menu_to_tui(depth + 1, &item.submenu, addr, menu_path)),
        ])
        .into()
    })
}

fn tray_menu_to_tui(
    depth: u16,
    items: &[system_tray::menu::MenuItem],
    addr: &Arc<str>,
    menu_path: Option<&Arc<str>>,
) -> tui::Elem {
    tui::Stack::vertical(items.iter().filter_map(|item| {
        Some(tui::StackItem {
            constr: tui::Constr::Auto,
            elem: tray_menu_item_to_tui(depth, item, addr, menu_path)?,
        })
    }))
    .into()
}

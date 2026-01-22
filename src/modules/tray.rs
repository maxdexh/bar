use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::Context;
use futures::{FutureExt, Stream};
use system_tray::item::StatusNotifierItem;
use tokio::task::JoinSet;
use tokio_stream::StreamExt as _;
use tokio_util::task::AbortOnDropHandle;

use crate::{
    modules::prelude::*,
    tui,
    utils::{
        Emit, ReloadRx, ReloadTx, ResultExt, SharedEmit, UnbTx, WatchRx, lossy_broadcast, unb_chan,
        watch_chan,
    },
};

#[derive(Debug, Clone)]
pub struct TrayMenuInteract {
    pub addr: Arc<str>,
    pub menu_path: Arc<str>,
    pub id: i32,
}
//match interact.kind {
//    tui::InteractKind::Hover => {
//        let items = state_rx.borrow().items.clone();

//        // FIXME: Handle more attrs
//        let Some(system_tray::item::Tooltip {
//            icon_name: _,
//            icon_data: _,
//            title,
//            description,
//        }) = items
//            .get(addr)
//            .and_then(|item| item.tool_tip.as_ref())
//        else {
//            log::error!("Unknown tray addr {addr}");
//            return;
//        };

//        let tui = tui::Stack::vertical([
//            tui::StackItem::auto(tui::Stack::horizontal([
//                tui::StackItem::new(
//                    tui::Constr::Fill(1),
//                    tui::Elem::empty(),
//                ),
//                tui::StackItem::auto(
//                    tui::PlainLines::new(title.as_str())
//                        .styled(tui::Style {
//                            modifier: tui::Modifier {
//                                bold: true,
//                                ..Default::default()
//                            },
//                            ..Default::default()
//                        }),
//                ),
//                tui::StackItem::new(
//                    tui::Constr::Fill(1),
//                    tui::Elem::empty(),
//                ),
//            ])),
//            tui::StackItem::auto(tui::PlainLines::new(
//                description.as_str(),
//            )),
//        ]);
//        act_tx.emit(ModuleAct::OpenMenu(OpenMenu {
//            monitor: interact.monitor,
//            tui: tui.into(),
//            location: interact.location,
//            menu_kind: MenuKind::Tooltip,
//        }));
//    }
//    tui::InteractKind::Click(tui::MouseButton::Right) => {
//        let menus = self.state_rx.borrow().menus.clone();
//        let Some(TrayMenuExt {
//            menu_path,
//            submenus,
//            ..
//        }) = menus.get(addr)
//        else {
//            log::error!("Unknown tray addr {addr}");
//            return;
//        };

//        let tui = tui::Block {
//            borders: tui::Borders::all(),
//            border_style: tui::Style {
//                fg: Some(tui::Color::DarkGrey),
//                ..Default::default()
//            },
//            border_set: tui::LineSet::thick(),
//            inner: Some(tray_menu_to_tui(
//                0,
//                submenus,
//                addr,
//                menu_path.as_ref(),
//            )),
//        };
//        act_tx.emit(ModuleAct::OpenMenu(OpenMenu {
//            monitor: interact.monitor,
//            tui: tui.into(),
//            location: interact.location,
//            menu_kind: MenuKind::Context,
//        }));
//    }
//    _ => (),
//};

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

pub struct TrayModule {
    state_rx: WatchRx<TrayState>,
    menu_interact_tx: UnbTx<TrayMenuInteract>,
    reload_tx: ReloadTx,
    _background: AbortOnDropHandle<()>,
}
impl TrayModule {
    async fn run_bg(
        state_tx: impl SharedEmit<TrayState>,
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
        state_tx: impl SharedEmit<TrayState>,
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
            state_tx.emit(TrayState { items, menus })
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
}

impl Module for TrayModule {
    type Config = ();

    fn connect() -> Self {
        let (state_tx, state_rx) = watch_chan(Default::default());
        let (menu_interact_tx, menu_interact_rx) = unb_chan();
        let reload_tx = ReloadTx::new();
        Self {
            _background: AbortOnDropHandle::new(tokio::spawn(Self::run_bg(
                state_tx,
                menu_interact_rx,
                reload_tx.subscribe(),
            ))),
            state_rx,
            menu_interact_tx,
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
        let mut reload_tx = self.reload_tx.clone();
        let reload_fut = reload_tx.reload_on(&mut reload_rx);
        let (bar_interact_tx, mut bar_interact_rx) = unb_chan();

        let bar_fut = async {
            let mut state_rx = self.state_rx.clone();
            while state_rx.changed().await.is_ok() {
                let items = state_rx.borrow_and_update().items.clone();
                let mut parts = Vec::new();
                for (addr, item) in items.iter() {
                    // FIXME: Handle the other options
                    // FIXME: Why are we showing all icons?
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

                        //payload: tui::InteractPayload {
                        //    mod_inst: inst_id.clone(),
                        //    tag: tui::InteractTag::new(TrayInteractTag {
                        //        addr: addr.clone(),
                        //    }),
                        //},
                        parts.extend([
                            tui::StackItem::auto(
                                tui::Elem::from(tui::Image {
                                    img,
                                    sizing: tui::ImageSizeMode::FillAxis(tui::Axis::Y, 1),
                                })
                                .on_interact_fn({
                                    let addr = addr.clone();
                                    let bar_interact_tx = bar_interact_tx.clone();
                                    move |interact| bar_interact_tx.emit((addr.clone(), interact))
                                }),
                            ),
                            tui::StackItem::spacing(1),
                        ])
                    }
                }
                let tui = tui::Stack::horizontal(parts);
                act_tx.emit(ModuleAct::RenderAll(tui::StackItem::auto(tui)));
            }
        };

        let interact_fut = async {
            let act_tx = act_tx.clone();
            while let Some((
                addr,
                tui::InteractData {
                    monitor,
                    location,
                    kind,
                    ..
                },
            )) = bar_interact_rx.next().await
            {
                match kind {
                    tui::InteractKind::Hover => {
                        let items = self.state_rx.borrow().items.clone();

                        // FIXME: Handle more attrs
                        let Some(system_tray::item::Tooltip {
                            icon_name: _,
                            icon_data: _,
                            title,
                            description,
                        }) = items.get(&addr).and_then(|item| item.tool_tip.as_ref())
                        else {
                            log::error!("Unknown tray addr {addr}");
                            continue;
                        };

                        let tui = tui::Stack::vertical([
                            tui::StackItem::auto(tui::Stack::horizontal([
                                tui::StackItem::new(tui::Constr::Fill(1), tui::Elem::empty()),
                                tui::StackItem::auto(tui::PlainLines::new(title.as_str()).styled(
                                    tui::Style {
                                        modifier: tui::Modifier {
                                            bold: true,
                                            ..Default::default()
                                        },
                                        ..Default::default()
                                    },
                                )),
                                tui::StackItem::new(tui::Constr::Fill(1), tui::Elem::empty()),
                            ])),
                            tui::StackItem::auto(tui::PlainLines::new(description.as_str())),
                        ]);
                        act_tx.emit(ModuleAct::OpenMenu(OpenMenu {
                            monitor,
                            tui: tui.into(),
                            location,
                            menu_kind: MenuKind::Tooltip,
                        }));
                    }
                    tui::InteractKind::Click(tui::MouseButton::Right) => {
                        let menus = self.state_rx.borrow().menus.clone();
                        let Some(TrayMenuExt {
                            menu_path,
                            submenus,
                            ..
                        }) = menus.get(&addr)
                        else {
                            log::error!("Unknown tray addr {addr}");
                            continue;
                        };

                        let tui = tui::Block {
                            borders: tui::Borders::all(),
                            border_style: tui::Style {
                                fg: Some(tui::Color::DarkGrey),
                                ..Default::default()
                            },
                            border_set: tui::LineSet::thick(),
                            inner: Some(tray_menu_to_tui(
                                0,
                                submenus,
                                menu_path
                                    .as_ref()
                                    .map(|menu_path| {
                                        |id| {
                                            let iact = TrayMenuInteract {
                                                addr: addr.clone(),
                                                menu_path: menu_path.clone(),
                                                id,
                                            };
                                            let menu_interact_tx = self.menu_interact_tx.clone();
                                            tui::InteractCallback::from_fn(move |interact| {
                                                if matches!(
                                                    interact.kind,
                                                    tui::InteractKind::Click(
                                                        tui::MouseButton::Left
                                                    )
                                                ) {
                                                    menu_interact_tx.emit(iact.clone());
                                                }
                                            })
                                        }
                                    })
                                    .as_ref(),
                            )),
                        };
                        act_tx.emit(ModuleAct::OpenMenu(OpenMenu {
                            monitor,
                            tui: tui.into(),
                            location,
                            menu_kind: MenuKind::Context,
                        }));
                    }
                    _ => continue,
                }
            }
        };

        tokio::select! {
            () = bar_fut => (),
            () = reload_fut => (),
            () = interact_fut => (),
        }
    }
}
fn tray_menu_item_to_tui(
    depth: u16,
    item: &system_tray::menu::MenuItem,
    on_interact: Option<&impl Fn(i32) -> tui::InteractCallback>,
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
            let elem = tui::Elem::from(tui::Stack::horizontal([
                tui::StackItem::spacing(depth + 1),
                if let Some(icon) = icon_data
                    && let Some(img) =
                        image::load_from_memory_with_format(icon, image::ImageFormat::Png)
                            .context("Systray icon has invalid png data")
                            .ok_or_log()
                {
                    let mut lines = label.lines();
                    let first_line = lines.next().unwrap_or_default();
                    tui::StackItem::auto(tui::Stack::vertical(Iterator::chain(
                        std::iter::once(tui::StackItem::length(
                            1,
                            tui::Stack::horizontal([
                                tui::StackItem::auto(tui::Image {
                                    img: img.into_rgba8(),
                                    sizing: tui::ImageSizeMode::FillAxis(tui::Axis::Y, 1),
                                }),
                                tui::StackItem::spacing(1),
                                tui::StackItem::auto(tui::PlainLines::new(first_line)),
                            ]),
                        )),
                        lines.map(tui::RawPrint::plain).map(tui::StackItem::auto),
                    )))
                } else {
                    tui::StackItem::auto(tui::PlainLines::new(label))
                },
                tui::StackItem::spacing(1),
            ]));

            //payload: tui::InteractPayload {
            //    mod_inst: inst_id.clone(),
            //    tag: tui::InteractTag::new(TrayMenuInteract {
            //        addr: addr.clone(),
            //        menu_path: it.clone(),
            //        id: *id,
            //    }),
            //},
            match on_interact {
                Some(it) => elem.on_interact(it(*id)),
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
            tui::StackItem::auto(tray_menu_to_tui(depth + 1, &item.submenu, on_interact)),
        ])
        .into()
    })
}

fn tray_menu_to_tui(
    depth: u16,
    items: &[system_tray::menu::MenuItem],
    on_interact: Option<&impl Fn(i32) -> tui::InteractCallback>,
) -> tui::Elem {
    tui::Stack::vertical(items.iter().filter_map(|item| {
        Some(tui::StackItem {
            constr: tui::Constr::Auto,
            elem: tray_menu_item_to_tui(depth, item, on_interact)?,
        })
    }))
    .into()
}

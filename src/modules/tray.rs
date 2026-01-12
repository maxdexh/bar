use std::{collections::HashMap, sync::Arc};

use anyhow::Context;
use futures::{FutureExt, Stream};
use system_tray::item::StatusNotifierItem;
use tokio::sync::broadcast;
use tokio_stream::StreamExt as _;

use crate::{
    modules::prelude::Module,
    tui,
    utils::{Emit, ReloadRx, ResultExt, lossy_broadcast},
};

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
    broadcast::Sender<TrayMenuInteract>, // FIXME: no
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

use crate::modules::prelude::*;
// FIXME: Refactor
pub struct Tray;

#[derive(Debug)]
struct Tag {
    addr: Arc<str>,
}
impl Module for Tray {
    async fn run_instance(
        &self,
        ModuleArgs {
            mut act_tx,
            upd_rx,
            reload_rx,
            ..
        }: ModuleArgs,
        _cancel: crate::utils::CancelDropGuard,
    ) {
        let (tx, rx) = connect(reload_rx);

        enum Upd {
            Tray(TrayState),
            Module(ModuleUpd),
        }
        let rx = rx.map(Upd::Tray).merge(upd_rx.map(Upd::Module));
        let mut cur_menus = Default::default();
        let mut cur_items = Default::default();
        tokio::pin!(rx);
        while let Some(upd) = rx.next().await {
            match upd {
                Upd::Tray(TrayState { items, menus }) => {
                    cur_menus = menus;
                    cur_items = items;

                    let mut parts = Vec::new();
                    for (addr, item) in cur_items.iter() {
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
                                    Arc::new(Tag { addr: addr.clone() }),
                                    tui::Image::load_or_empty(png_data, image::ImageFormat::Png),
                                )),
                                tui::StackItem::spacing(1),
                            ])
                        }
                    }
                    let tui = tui::Stack::horizontal(parts);
                    act_tx.emit(ModuleAct::RenderAll(tui.into()));
                }
                Upd::Module(ModuleUpd::Interact(ModuleInteract {
                    location,
                    payload,
                    kind,
                })) => {
                    if let Some(Tag { addr }) = payload.tag.downcast_ref() {
                        let menu_kind;
                        let tui;
                        match kind {
                            tui::InteractKind::Hover => {
                                let Some(system_tray::item::Tooltip {
                                    icon_name: _,
                                    icon_data: _,
                                    title,
                                    description,
                                }) = cur_items
                                    .iter()
                                    .find(|(a, _)| a == addr)
                                    .and_then(|(_, item)| item.tool_tip.as_ref())
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
                                let Some(TrayMenuExt {
                                    menu_path,
                                    submenus,
                                    ..
                                }) = cur_menus.iter().find(|(a, _)| a == addr).map(|(_, it)| it)
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
                            monitor: payload.monitor,
                            tui,
                            pos: location,
                            menu_kind,
                        }));
                    }
                }
            }
        }
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

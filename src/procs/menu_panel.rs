use std::{collections::HashMap, ffi::OsString, ops::ControlFlow, sync::Arc, time::Duration};

use anyhow::Context as _;
use futures::Stream;
use serde::{Deserialize, Serialize};
use system_tray::item::Tooltip;
use tokio::task::JoinSet;
use tokio_stream::StreamExt;
use tokio_util::time::FutureExt as _;

use crate::{
    clients::{
        monitors::{MonitorEvent, MonitorInfo},
        tray::{TrayMenuExt, TrayMenuInteract},
    },
    data::Position32,
    terminals::{SpawnTerm, TermEvent, TermId, TermMgrUpdate, TermUpdate},
    tui,
    utils::{Emit, ResultExt as _, SharedEmit, unb_chan, unb_rx_stream},
};

#[derive(Serialize, Deserialize, Debug)]
pub enum MenuEvent {
    Interact(Interact),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum MenuWatcherEvent {
    Hide,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum MenuUpdate {
    UnfocusMenu,
    Hide,
    SwitchSubject {
        new_menu: Menu,
        location: Position32,
        monitor: Arc<str>,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum MenuInteractTarget {
    TrayMenu(crate::clients::tray::TrayMenuInteract),
}
impl MenuInteractTarget {
    fn serialize_tag(&self) -> tui::InteractTag {
        tui::InteractTag::from_bytes(&postcard::to_stdvec(self).unwrap())
    }
    fn deserialize_tag(tag: &tui::InteractTag) -> Self {
        postcard::from_bytes(tag.as_bytes()).unwrap()
    }
}

// FIXME: Send finished tui instead, do conversion in client
// Also send extra info like close_on_unfocus/is_tooltip.
#[derive(Serialize, Deserialize, Debug)]
pub enum Menu {
    TrayContext { addr: Arc<str>, tmenu: TrayMenuExt },
    TrayTooltip { addr: Arc<str>, tooltip: Tooltip },
}
impl Menu {
    fn close_on_unfocus(&self) -> bool {
        match self {
            Self::TrayTooltip { .. } => true,
            Self::TrayContext { .. } => false,
        }
    }
}

type Interact = crate::data::InteractGeneric<MenuInteractTarget>;

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
                                tui::StackItem::auto(tui::Image {
                                    data: icon.clone(),
                                    format: image::ImageFormat::Png,
                                    cached: None,
                                }),
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
                    tag: MenuInteractTarget::TrayMenu(TrayMenuInteract {
                        addr: addr.clone(),
                        menu_path: it.clone(),
                        id: *id,
                    })
                    .serialize_tag(),
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

pub fn to_tui(menu: &Menu) -> tui::Tui {
    match menu {
        Menu::TrayContext {
            addr,
            tmenu:
                TrayMenuExt {
                    id: _,
                    menu_path,
                    submenus,
                },
        } => {
            // Then render the items
            tui::Tui {
                root: Box::new(
                    tui::Block {
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
                    .into(),
                ),
            }
        }
        Menu::TrayTooltip {
            addr: _,
            tooltip:
                Tooltip {
                    icon_name: _,
                    icon_data: _,
                    title,
                    description,
                },
        } => tui::Tui {
            root: Box::new(
                tui::Stack::vertical([
                    tui::StackItem::auto(tui::Stack::horizontal([
                        tui::StackItem::new(tui::Constr::Fill(1), tui::Elem::Empty),
                        tui::StackItem::auto(tui::Text::plain(title.as_str()).styled(tui::Style {
                            modifier: tui::Modifier {
                                bold: true,
                                ..Default::default()
                            },
                            ..Default::default()
                        })),
                        tui::StackItem::new(tui::Constr::Fill(1), tui::Elem::Empty),
                    ])),
                    tui::StackItem::auto(tui::Text::plain(description.as_str())),
                ])
                .into(),
            ),
        },
    }
}

pub async fn run_menu_panel_manager(
    monitor_rx: impl Stream<Item = MonitorEvent> + Send + 'static,
    menu_upd_rx: impl Stream<Item = MenuUpdate> + Send + 'static,
    menu_ev_tx: impl SharedEmit<MenuEvent>,
) {
    let mut tasks = JoinSet::<()>::new();

    let mut term_upd_tx;
    {
        let term_upd_rx;
        (term_upd_tx, term_upd_rx) = unb_chan();
        tasks.spawn(crate::terminals::run_term_manager(term_upd_rx));
    }
    let (watcher_ev_tx, watcher_ev_rx) = unb_chan();

    // TODO: Move to function
    tasks.spawn(async move {
        struct Instance {
            _tmpdir_guard: tempfile::TempDir,
            inst_task: tokio::task::AbortHandle,
            bar_upd_tx: tokio::sync::mpsc::UnboundedSender<Option<PanelShow>>,
        }
        let mut instances = HashMap::<TermId, Instance>::new();
        let monitor_to_term_id = |name: &str| TermId::from_bytes(name.as_bytes());
        let mut global_inst_tasks = JoinSet::<()>::new();

        #[derive(Debug)]
        enum Upd {
            Monitor(MonitorEvent),
            Menu(MenuUpdate),
            Watcher((Arc<str>, MenuWatcherEvent)),
        }
        let updates = monitor_rx
            .map(Upd::Monitor)
            .merge(menu_upd_rx.map(Upd::Menu))
            .merge(watcher_ev_rx.map(Upd::Watcher));
        tokio::pin!(updates);

        struct Current {
            menu: Menu,
            term: TermId,
        }
        let mut cur = None::<Current>;
        fn shutdown(
            instances: &mut HashMap<TermId, Instance>,
            ids: impl IntoIterator<Item = TermId>,
            term_upd_tx: &mut impl Emit<TermMgrUpdate>,
        ) -> ControlFlow<()> {
            for id in ids {
                if let Some(inst) = instances.remove(&id) {
                    inst.inst_task.abort();
                    term_upd_tx.emit(TermMgrUpdate::TermUpdate(id, TermUpdate::Shutdown))?
                }
            }
            ControlFlow::Continue(())
        }
        loop {
            let upd = tokio::select! {
                Some(upd) = updates.next() => upd,
                Some(res) = global_inst_tasks.join_next() => {
                    if let Err(err) = res && !err.is_cancelled() {
                        log::error!("Error with task: {err}");
                    }
                    continue;
                }
            };

            let mut hide = |cur: &mut Option<Current>| {
                if let Some(Current { term, .. }) = cur.take()
                    && let Some(inst) = instances.get_mut(&term)
                {
                    if inst.bar_upd_tx.emit(None).is_break() {
                        shutdown(&mut instances, [term], &mut term_upd_tx)?;
                    }
                }
                ControlFlow::Continue(())
            };

            match upd {
                Upd::Menu(MenuUpdate::UnfocusMenu) => {
                    if cur.as_ref().is_some_and(|it| it.menu.close_on_unfocus()) {
                        if hide(&mut cur).is_break() {
                            break;
                        }
                    }
                }
                Upd::Watcher((monitor, MenuWatcherEvent::Hide)) => {
                    let term_id = monitor_to_term_id(&monitor);
                    if cur.as_ref().is_some_and(|it| it.term == term_id) {
                        if hide(&mut cur).is_break() {
                            break;
                        }
                    } else if let Some(inst) = instances.get_mut(&term_id) {
                        if inst.bar_upd_tx.emit(None).is_break() {
                            if shutdown(&mut instances, [term_id], &mut term_upd_tx).is_break() {
                                break;
                            }
                        }
                    }
                }
                Upd::Menu(MenuUpdate::SwitchSubject {
                    new_menu,
                    location,
                    monitor,
                }) => {
                    // Never replace a context menu with a tooltip
                    if new_menu.close_on_unfocus()
                        && cur.as_ref().is_some_and(|it| !it.menu.close_on_unfocus())
                    {
                        continue;
                    }

                    let term = monitor_to_term_id(&monitor);
                    if cur.as_ref().is_some_and(|it| it.term != term) {
                        if hide(&mut cur).is_break() {
                            break;
                        }
                    }
                    if let Some(inst) = instances.get_mut(&term) {
                        let tui = to_tui(&new_menu);
                        if inst
                            .bar_upd_tx
                            .emit(Some(PanelShow { tui, pos: location }))
                            .is_break()
                        {
                            if shutdown(&mut instances, [term], &mut term_upd_tx).is_break() {
                                break;
                            }
                            continue;
                        }
                    }
                    cur = Some(Current {
                        menu: new_menu,
                        term,
                    });
                }
                Upd::Menu(MenuUpdate::Hide) => {
                    if hide(&mut cur).is_break() {
                        break;
                    }
                }
                // TODO: Move into function
                Upd::Monitor(ev) => {
                    if shutdown(
                        &mut instances,
                        ev.removed()
                            .chain(ev.added_or_changed().map(|it| &it.name as &str))
                            .map(monitor_to_term_id),
                        &mut term_upd_tx,
                    )
                    .is_break()
                    {
                        break;
                    };

                    for monitor in ev.added_or_changed().cloned() {
                        let term_id = TermId::from_bytes(monitor.name.as_bytes());
                        let (bar_upd_tx, bar_upd_rx) = tokio::sync::mpsc::unbounded_channel();
                        let (term_ev_tx, term_ev_rx) = tokio::sync::mpsc::unbounded_channel();

                        let (tmpdir, watcher_py, watcher_sock_path, watcher_sock) = {
                            let res = tokio::task::spawn_blocking(|| {
                                let tmpdir = tempfile::TempDir::new()?;
                                let watcher_py = tmpdir.path().join("menu_watcher.py");
                                std::fs::write(
                                    &watcher_py,
                                    include_bytes!("menu_panel/menu_watcher.py"),
                                )?;

                                let sock_path = tmpdir.path().join("menu_watcher.sock");
                                let sock = tokio::net::UnixListener::bind(&sock_path)?;
                                Ok((tmpdir, watcher_py, sock_path, sock))
                            })
                            .await;
                            match res.map_err(anyhow::Error::from).flatten() {
                                Ok(tup) => tup,
                                Err(err) => {
                                    log::error!("Failed to spawn menu: {err}");
                                    continue;
                                }
                            }
                        };
                        let mut inst_subtasks = JoinSet::<()>::new();
                        inst_subtasks.spawn(run_instance_mgr(
                            menu_ev_tx.clone(),
                            unb_rx_stream(bar_upd_rx),
                            {
                                let mut term_upd_tx = term_upd_tx.clone();
                                let term_id = term_id.clone();
                                move |upd| {
                                    term_upd_tx
                                        .emit(TermMgrUpdate::TermUpdate(term_id.clone(), upd))
                                }
                            },
                            unb_rx_stream(term_ev_rx),
                            monitor.clone(),
                        ));

                        // FIXME: Move to function
                        let mut watcher_ev_tx = watcher_ev_tx.clone();
                        let monitor_name = monitor.name.clone();
                        inst_subtasks.spawn(async move {
                            let Ok((mut watcher_stream, _)) =
                                watcher_sock.accept().await.map_err(|err| {
                                    log::error!("Failed to accept connection to watcher: {err}")
                                })
                            else {
                                return;
                            };

                            use tokio::io::AsyncReadExt as _;
                            loop {
                                let Ok(byte) = watcher_stream.read_u8().await.map_err(|err| {
                                    log::error!("Failed to read from watcher stream: {err}")
                                }) else {
                                    break;
                                };
                                let parsed = match byte {
                                    0 => MenuWatcherEvent::Hide,
                                    _ => {
                                        log::error!("Unknown watcher event {byte}");
                                        continue;
                                    }
                                };

                                if watcher_ev_tx
                                    .emit((monitor_name.clone(), parsed))
                                    .is_break()
                                {
                                    break;
                                }
                            }
                        });
                        if term_upd_tx
                            .emit(TermMgrUpdate::SpawnPanel(SpawnTerm {
                                term_id: term_id.clone(),
                                extra_args: vec![
                                    {
                                        let mut arg = OsString::from("-o=watcher=");
                                        arg.push(watcher_py);
                                        arg
                                    },
                                    format!("--output-name={}", monitor.name).into(),
                                    // Configure remote control via socket
                                    "-o=allow_remote_control=socket-only".into(),
                                    "--listen-on=unix:/tmp/kitty-bar-menu-panel.sock".into(),
                                    // Allow logging to $KITTY_STDIO_FORWARDED
                                    "-o=forward_stdio=yes".into(),
                                    // Do not use the system's kitty.conf
                                    "--config=NONE".into(),
                                    // Basic look of the menu
                                    "-o=background_opacity=0.85".into(),
                                    "-o=background=black".into(),
                                    "-o=foreground=white".into(),
                                    // location of the menu
                                    "--edge=top".into(),
                                    // disable hiding the mouse
                                    "-o=mouse_hide_wait=0".into(),
                                    // Window behavior of the menu panel. Makes panel
                                    // act as an overlay on top of other windows.
                                    // We do not want tilers to dedicate space to it.
                                    // Taken from the args that quick-access-terminal uses.
                                    "--exclusive-zone=0".into(),
                                    "--override-exclusive-zone".into(),
                                    "--layer=overlay".into(),
                                    // Focus behavior of the panel. Since we cannot tell from
                                    // mouse events alone when the cursor leaves the panel
                                    // (since terminal mouse capture only gives us mouse
                                    // events inside the panel), we need external support for
                                    // hiding it automatically. We use a watcher to be able
                                    // to reset the menu state when this happens.
                                    "--focus-policy=on-demand".into(),
                                    "--hide-on-focus-loss".into(),
                                    // Since we control resizes from the program and not from
                                    // a somewhat continuous drag-resize, debouncing between
                                    // resize and reloads is completely inappropriate and
                                    // just results in a larger delay between resize and
                                    // the old menu content being replaced with the new one.
                                    "-o=resize_debounce_time=0 0".into(),
                                    // TODO: Mess with repaint_delay, input_delay
                                ],
                                extra_envs: vec![(
                                    "BAR_MENU_WATCHER_SOCK".into(),
                                    watcher_sock_path.into(),
                                )],
                                term_ev_tx,
                            }))
                            .is_break()
                        {
                            break;
                        }

                        let old = instances.insert(
                            term_id,
                            Instance {
                                _tmpdir_guard: tmpdir,
                                inst_task: global_inst_tasks.spawn(async move {
                                    inst_subtasks
                                        .join_next()
                                        .await
                                        .transpose()
                                        .context("Task exited with error")
                                        .ok_or_log();
                                }),
                                bar_upd_tx,
                            },
                        );
                        assert!(old.is_none());
                    }
                }
            }
        }
    });

    if let Some(Err(err)) = tasks.join_next().await {
        log::error!("Error with task: {err}");
    }
}

struct PanelShow {
    tui: tui::Tui,
    pos: Position32,
}
async fn run_instance_mgr(
    mut ev_tx: impl SharedEmit<MenuEvent>,
    upd_rx: impl Stream<Item = Option<PanelShow>> + 'static + Send,
    mut term_upd_tx: impl SharedEmit<TermUpdate>,
    term_ev_rx: impl Stream<Item = TermEvent> + 'static + Send,
    monitor: MonitorInfo,
) {
    tokio::pin!(term_ev_rx);

    let Some(mut sizes) = async {
        loop {
            if let Some(TermEvent::Sizes(sizes)) = term_ev_rx.next().await {
                break sizes;
            }
        }
    }
    .timeout(Duration::from_secs(10))
    .await
    .context("Failed to receive terminal sizes")
    .ok_or_log() else {
        return;
    };

    enum Inc {
        Hide,
        Show(PanelShow),
        Term(TermEvent),
    }
    let incoming = upd_rx
        .map(|maybe_show| maybe_show.map_or(Inc::Hide, Inc::Show))
        .merge(term_ev_rx.map(Inc::Term));
    tokio::pin!(incoming);

    struct Show {
        tui: tui::Tui,
        pos: Position32,
        tui_size_cache: tui::Vec2<u16>,
        rendered: bool,
    }
    let mut show = None;
    let mut layout = tui::RenderedLayout::default();

    let siz_ctx = |sizes: tui::Sizes| tui::SizingContext {
        font_size: sizes.font_size(),
        div_w: None,
        div_h: None,
    };

    while let Some(inc) = incoming.next().await {
        let mut resize = false;
        let mut render_ready = false;
        match inc {
            Inc::Show(PanelShow { mut tui, pos }) => {
                if let Ok(cached_size) = tui
                    .calc_size(siz_ctx(sizes))
                    .map_err(|err| log::error!("Failed to calculate tui size: {err}"))
                {
                    show = Some(Show {
                        tui_size_cache: cached_size,
                        tui,
                        pos,
                        rendered: false,
                    });
                    resize = true;
                }
            }
            Inc::Hide => {
                show = None;
                resize = true;
            }
            Inc::Term(TermEvent::Sizes(new_sizes)) => {
                sizes = new_sizes;
                render_ready = true;
            }
            Inc::Term(TermEvent::Crossterm(ev)) => {
                if let crossterm::event::Event::Mouse(ev) = ev
                    && let Some(tui::TuiInteract {
                        location,
                        target: Some(tag),
                        kind,
                    }) = layout.interpret_mouse_event(ev, sizes.font_size())
                {
                    let interact = Interact {
                        location,
                        kind,
                        target: MenuInteractTarget::deserialize_tag(&tag),
                    };
                    if ev_tx.emit(MenuEvent::Interact(interact)).is_break() {
                        break;
                    }
                }
            }
        }

        if resize {
            if let Some(Show {
                pos,
                tui_size_cache,
                rendered: false,
                ..
            }) = show
            {
                let scale = (monitor.scale * 1000.0).ceil() / 1000.0;

                // No need to wait before rendering if we have enough space
                if tui_size_cache.x <= sizes.cell_size.x && tui_size_cache.y <= sizes.cell_size.y {
                    render_ready = true;
                }
                if tui_size_cache != sizes.cell_size {
                    // NOTE: There is no absolute positioning system, nor a way to directly specify the
                    // geometry (since this is controlled by the compositor). So we have to get creative by
                    // using the right and left margin to control both position and size of the panel.

                    // cap position at monitor's size
                    let x = std::cmp::min(pos.x, monitor.width);

                    // Find the distance between window edge and center
                    let half_pix_w =
                        (u32::from(tui_size_cache.x) * u32::from(sizes.font_size().x)).div_ceil(2);

                    // The left margin should be such that half the space is between
                    // left margin and x. Use saturating_sub so that the left
                    // margin becomes zero if the width would reach outside the screen.
                    let mleft = x.saturating_sub(half_pix_w);

                    // Get the overshoot, i.e. the amount lost to saturating_sub (we have to account for it
                    // in the right margin). For this observe that a.saturating_sub(b) = a - min(a, b) and
                    // therefore the overshoot is:
                    // a.saturating_sub(b) - (a - b)
                    // = a - min(a, b) - (a - b)
                    // = b - min(a, b)
                    // = b.saturating_sub(a)
                    let overshoot = half_pix_w.saturating_sub(x);

                    let mright = (monitor.width - x)
                        .saturating_sub(half_pix_w)
                        .saturating_sub(overshoot);

                    // The font size (on which cell->pixel conversion is based) and the monitor's
                    // size are in physical pixels. This makes sense because different monitors can
                    // have different scales, and the application should not be affected by that
                    // (this is not x11 after all).
                    // However, panels are bound to a monitor and the margins are in scaled pixels,
                    // so we have to make this correction.
                    let margin_left = (f64::from(mleft) / scale) as u32;
                    let margin_right = (f64::from(mright) / scale) as u32;

                    if term_upd_tx
                        .emit(TermUpdate::RemoteControl(vec![
                            "resize-os-window".into(),
                            "--incremental".into(),
                            "--action=os-panel".into(),
                            format!("margin-left={margin_left}").into(),
                            format!("margin-right={margin_right}").into(),
                            format!("lines={}", tui_size_cache.y).into(),
                        ]))
                        .is_break()
                    {
                        break;
                    };
                }
            }

            // TODO: be smarter about when to run this
            let action = if show.is_some() { "show" } else { "hide" };
            if term_upd_tx
                .emit(TermUpdate::RemoteControl(vec![
                    "resize-os-window".into(),
                    format!("--action={}", action).into(),
                ]))
                .is_break()
            {
                break;
            }
        }
        if render_ready
            && let Some(Show {
                ref mut tui,
                rendered: ref mut rendered @ false,
                tui_size_cache,
                ..
            }) = show
        {
            if tui_size_cache.x > sizes.cell_size.x || tui_size_cache.y > sizes.cell_size.y {
                log::warn!(
                    "Tui size {tui_size_cache:?} is too big for panel size {:?}",
                    sizes.cell_size
                );
            }

            let mut buf = Vec::new();
            match tui::draw_to(&mut buf, |ctx| {
                let size = tui_size_cache; // Or term size
                tui.render(
                    ctx,
                    tui::SizingContext {
                        font_size: sizes.font_size(),
                        div_w: Some(size.x),
                        div_h: Some(size.y),
                    },
                    tui::Area {
                        // FIXME: probably better to render at the tui's size
                        size,
                        pos: Default::default(),
                    },
                )
            }) {
                Err(err) => log::error!("Failed to draw: {err}"),
                Ok(new_layout) => {
                    layout = new_layout;
                    *rendered = true;
                }
            }
            if term_upd_tx.emit(TermUpdate::Print(buf)).is_break()
                || term_upd_tx.emit(TermUpdate::Flush).is_break()
            {
                break;
            }
        }
    }
}

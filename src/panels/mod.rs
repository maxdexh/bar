pub(crate) mod proc;

use proc::{TermEvent, TermUpdate};

use std::{collections::HashMap, ffi::OsString, sync::Arc, time::Duration};

use anyhow::Context;
use futures::Stream;
use futures::StreamExt;
use tokio::task::JoinSet;
use tokio_util::{sync::CancellationToken, time::FutureExt as _};

use crate::{
    monitors::MonitorInfo,
    tui,
    utils::{CancelDropGuard, ReloadTx, ResultExt, UnbRx, UnbTx, WatchRx, unb_chan, watch_chan},
};

// FIXME: Add to args of run_manager
const EDGE: &str = "top";

/// Adds an extra line and centers the content of the menu with padding of half a cell.
const VERTICAL_PADDING: bool = true;
const HORIZONTAL_PADDING: u16 = 4;

#[derive(Clone, Debug)]
pub struct OpenMenu {
    pub monitor: Arc<str>,
    pub tui: tui::Elem,
    pub pix_location: tui::Vec2<u32>,
    pub menu_kind: MenuKind,
}
#[derive(Debug, Clone, Copy)]
pub enum MenuKind {
    Tooltip,
    Context,
}

// FIXME: Take Vec<StackItem> (hide becomes empty list), add helper methods
#[derive(Debug, Clone)]
pub enum BarTuiElem {
    Shared(tui::StackItem),
    ByMonitor(HashMap<Arc<str>, tui::StackItem>),
    Hide,
}
fn gather_bar_tui(bar_tui: &[BarTuiElem], monitor: &MonitorInfo) -> tui::Elem {
    let mut parts = Vec::new();
    for elem in bar_tui {
        let elem = match elem {
            BarTuiElem::Shared(elem) => Some(elem.clone()),
            BarTuiElem::ByMonitor(elems) => elems.get(&monitor.name).cloned(),
            BarTuiElem::Hide => None,
        };
        parts.extend(elem);
    }
    parts.push(tui::StackItem::spacing(1));
    tui::Stack::horizontal(parts).into()
}

pub async fn run_manager(
    bar_rx: WatchRx<Vec<WatchRx<BarTuiElem>>>,
    menu_rx: impl Stream<Item = OpenMenu>,
    mut reload_tx: ReloadTx,
) {
    tokio::pin!(menu_rx);
    let mut monitors = HashMap::<_, (_, CancelDropGuard)>::new();

    let mut monitor_rx = crate::monitors::connect();

    loop {
        tokio::select! {
            Some(ev) = monitor_rx.next() => {
                for monitor in ev.removed() {
                    monitors.remove(monitor);
                }
                for monitor in ev.added_or_changed() {
                    let cancel = CancellationToken::new();
                    let (menu_tx, menu_rx) = unb_chan();
                    tokio::spawn(run_monitor(
                        monitor.clone(),
                        cancel.clone(),
                        bar_rx.clone(),
                        menu_rx,
                        reload_tx.clone(),
                    ));
                    monitors.insert(monitor.name.clone(), (menu_tx, cancel.into()));
                }
                reload_tx.reload();
            }
            Some(menu) = menu_rx.next() => {
                let Some((menu_tx, _)) = monitors.get(&menu.monitor) else {
                    continue;
                };
                menu_tx.send(menu).ok_or_log();
            }
        }
    }
}

async fn run_monitor(
    monitor: MonitorInfo,
    cancel_monitor: CancellationToken,
    bar_rx: WatchRx<Vec<WatchRx<BarTuiElem>>>,
    menu_rx: impl Stream<Item = OpenMenu>,
    mut reload_tx: ReloadTx,
) {
    let _auto_cancel = CancelDropGuard::from(cancel_monitor.clone());
    tokio::pin!(menu_rx);

    struct Term {
        term_ev_rx: UnbRx<TermEvent>,
        term_upd_tx: UnbTx<TermUpdate>,
        sizes: tui::Sizes,
        layout: Option<tui::RenderedLayout>,
    }
    #[derive(Debug, Clone, Copy)]
    enum TermKind {
        Menu,
        Bar,
    }
    enum Upd {
        BarTui,
        MenuWatcherHide,
        Term(TermKind, TermEvent),
        OpenMenu(OpenMenu),
    }

    let mut try_run = async || -> anyhow::Result<std::convert::Infallible> {
        let cancel = cancel_monitor.child_token();
        let _auto_cancel = CancelDropGuard::from(cancel.clone());
        let mut subtasks = JoinSet::<anyhow::Result<std::convert::Infallible>>::new();
        let (intern_upd_tx, mut intern_upd_rx) = unb_chan();

        let init_term = async |log_name: String, args, envs| {
            let (term_upd_tx, term_upd_rx) = unb_chan();
            let (term_ev_tx, mut term_ev_rx) = unb_chan();

            proc::spawn_generic_panel(
                &log_name,
                term_upd_rx,
                args,
                envs,
                term_ev_tx,
                cancel.clone(),
            )?;
            let sizes = async {
                loop {
                    if let Some(TermEvent::Sizes(sizes)) = term_ev_rx.next().await {
                        break sizes;
                    }
                }
            }
            .await;

            anyhow::Ok(Term {
                sizes,
                layout: Default::default(),
                term_ev_rx,
                term_upd_tx,
            })
        };

        let (_tmpdir, watcher_py, watcher_sock_path, watcher_sock) =
            tokio::task::spawn_blocking(|| {
                let tmpdir = tempfile::TempDir::new()?;
                let watcher_py = tmpdir.path().join("menu_watcher.py");
                std::fs::write(&watcher_py, include_bytes!("menu_watcher.py"))?;

                let sock_path = tmpdir.path().join("menu_watcher.sock");
                let sock = tokio::net::UnixListener::bind(&sock_path)?;
                Ok((tmpdir, watcher_py, sock_path, sock))
            })
            .await
            .map_err(anyhow::Error::from)
            .flatten()?;

        let (mut bar, (mut menu, mut watcher_stream)) = futures::future::try_join(
            async {
                init_term(
                    format!("BAR@{}", monitor.name),
                    vec![
                        format!("--output-name={}", monitor.name).into(),
                        // Allow logging to $KITTY_STDIO_FORWARDED
                        "-o=forward_stdio=yes".into(),
                        // Do not use the system's kitty.conf
                        "--config=NONE".into(),
                        // Basic look of the bar
                        "-o=foreground=white".into(),
                        "-o=background=black".into(),
                        // location of the bar
                        format!("--edge={}", crate::panels::EDGE).into(),
                        // disable hiding the mouse
                        "-o=mouse_hide_wait=0".into(),
                    ],
                    vec![],
                )
                .await
            },
            async {
                let menu = init_term(
                    format!("MENU@{}", monitor.name),
                    vec![
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
                        // Center within leftover pixels if cell size does not divide window size.
                        "-o=placement_strategy=center".into(),
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
                    vec![("BAR_MENU_WATCHER_SOCK", watcher_sock_path)],
                )
                .await?;
                menu.term_upd_tx
                    .send(TermUpdate::RemoteControl(vec![
                        "resize-os-window".into(),
                        "--action=hide".into(),
                    ]))
                    .ok_or_log();
                if VERTICAL_PADDING {
                    // HACK: For some reason, using half font height padding at top and bottom
                    // shrinks the height by 2 cells. This way of doing it only works assuming
                    // that we do not have more than 1 pixel to spare for the padding and it
                    // can only be used for vertical padding of 1 cell in total.
                    menu.term_upd_tx
                        .send(TermUpdate::RemoteControl(vec![
                            "set-spacing".into(),
                            "padding-top=1".into(),
                            "padding-bottom=1".into(),
                        ]))
                        .ok_or_log();
                }

                let (s, _) = watcher_sock.accept().await?;
                anyhow::Ok((menu, s))
            },
        )
        .timeout(Duration::from_secs(10))
        .await??;

        subtasks.spawn({
            let upd_tx = intern_upd_tx.clone();
            async move {
                use tokio::io::AsyncReadExt as _;
                loop {
                    let byte = watcher_stream
                        .read_u8()
                        .await
                        .context("Failed to read from watcher stream")?;

                    let parsed = match byte {
                        0 => Upd::MenuWatcherHide,
                        _ => {
                            log::error!("Unknown watcher event {byte}");
                            continue;
                        }
                    };

                    upd_tx.send(parsed).ok_or_log();
                }
            }
        });

        let (bar_tui_tx, mut bar_tui_rx) = watch_chan(Vec::new());
        let mut bar_rx = bar_rx.clone();
        subtasks.spawn(async move {
            loop {
                let mut current_bar = bar_rx.borrow_and_update().clone();
                for it in current_bar.iter_mut() {
                    it.mark_changed();
                }
                let mut tasks = JoinSet::new();

                bar_tui_tx.send_if_modified(|tui| {
                    *tui = std::iter::repeat_n(BarTuiElem::Hide, current_bar.len()).collect();
                    false
                });

                let (bar_changed_tx, mut bar_changed_rx) = unb_chan();
                for (i, mut rx) in current_bar.into_iter().enumerate() {
                    let bar_changed_tx = bar_changed_tx.clone();
                    tasks.spawn(async move {
                        while let Ok(()) = rx.changed().await {
                            _ = bar_changed_tx.send((i, rx.borrow_and_update().clone()));
                        }
                    });
                }

                loop {
                    tokio::select! {
                        res = bar_rx.changed() => {
                            res?;
                            break; // restart the listener
                        }
                        Some((i, elem)) = bar_changed_rx.next() => {
                            bar_tui_tx.send_modify(|tui| {
                                tui[i] = elem;
                                while let Ok((i, elem)) = bar_changed_rx.inner.try_recv() {
                                    tui[i] = elem;
                                }
                            });
                        }
                    }
                }
            }
        });

        #[derive(Debug)]
        struct ShowMenu {
            tui: tui::Elem,
            kind: MenuKind,
            location: tui::Vec2<u32>,
            cached_tui_size: tui::Vec2<u16>,
            sizing: tui::SizingArgs,
            rendered: bool,
        }
        let mut show_menu = None::<ShowMenu>;
        loop {
            let mut resize_menu = false;

            let upd = tokio::select! {
                Some(ev) = bar.term_ev_rx.next() => Upd::Term(TermKind::Bar, ev),
                Some(ev) = menu.term_ev_rx.next() => Upd::Term(TermKind::Menu, ev),
                Some(upd) = intern_upd_rx.next() => upd,
                Some(menu) = menu_rx.next() => Upd::OpenMenu(menu),
                Ok(()) = bar_tui_rx.changed() => Upd::BarTui,
                Some(res) = subtasks.join_next() => return res?,
            };
            match upd {
                Upd::BarTui => {
                    let tui = gather_bar_tui(&bar_tui_rx.borrow_and_update(), &monitor);
                    let mut buf = Vec::new();
                    let Some(layout) = tui::render(
                        &tui,
                        tui::Area {
                            size: bar.sizes.cell_size,
                            pos: Default::default(),
                        },
                        &mut buf,
                        &tui::SizingArgs {
                            font_size: bar.sizes.font_size(),
                        },
                    )
                    .context("Failed to render bar")
                    .ok_or_log() else {
                        continue;
                    };
                    bar.layout = Some(layout);

                    bar.term_upd_tx.send(TermUpdate::Print(buf)).ok_or_log();
                    bar.term_upd_tx.send(TermUpdate::Flush).ok_or_log();
                }
                Upd::Term(term_kind, ev) => match ev {
                    TermEvent::Crossterm(ev) => match ev {
                        crossterm::event::Event::Mouse(ev) => {
                            if let Some((interact, callback)) = bar.layout.as_ref().and_then(|it| {
                                it.interpret_mouse_event(
                                    ev,
                                    bar.sizes.font_size(),
                                    monitor.name.clone(),
                                )
                            }) {
                                if let Some(callback) = callback {
                                    tokio::task::spawn_blocking(move || callback.call(interact));
                                } else if matches!(term_kind, TermKind::Bar) {
                                    let hide = match interact.kind {
                                        tui::InteractKind::Hover => {
                                            show_menu.as_ref().is_some_and(|it| match it.kind {
                                                MenuKind::Tooltip => true,
                                                MenuKind::Context => false,
                                            })
                                        }
                                        tui::InteractKind::Click(..)
                                        | tui::InteractKind::Scroll(..) => true,
                                    };
                                    if hide {
                                        show_menu = None;
                                        resize_menu = true;
                                    }
                                }
                            }
                        }
                        _ => {
                            //
                        }
                    },
                    TermEvent::Sizes(sizes) => match term_kind {
                        TermKind::Bar => {
                            bar.sizes = sizes;
                            reload_tx.reload();
                        }
                        TermKind::Menu => {
                            if menu.sizes.font_size() != sizes.font_size()
                                && let Some(show) = &mut show_menu
                            {
                                show.rendered = false;
                                menu.sizes = sizes;
                            } else {
                                menu.sizes = sizes;
                            }
                        }
                    },
                },
                Upd::OpenMenu(OpenMenu {
                    tui,
                    pix_location: location,
                    menu_kind,
                    monitor: _,
                }) => {
                    let sizing = tui::SizingArgs {
                        font_size: menu.sizes.font_size(),
                    };
                    show_menu = Some(ShowMenu {
                        cached_tui_size: tui::calc_min_size(&tui, &sizing),
                        sizing,
                        tui,
                        kind: menu_kind,
                        location,
                        rendered: false,
                    });
                    resize_menu = true;
                }
                Upd::MenuWatcherHide => {
                    show_menu = None;
                }
            }

            if resize_menu {
                if let Some(ShowMenu {
                    location,
                    cached_tui_size,
                    rendered: false,
                    ..
                }) = show_menu
                    && cached_tui_size != menu.sizes.cell_size
                {
                    // HACK: This minimizes the rounding error for some reason (as far as I can tell).
                    let scale = (monitor.scale * 1000.0).ceil() / 1000.0;

                    // NOTE: There is no absolute positioning system, nor a way to directly specify the
                    // geometry (since this is controlled by the compositor). So we have to get creative by
                    // using the right and left margin to control both position and size of the panel.

                    let lines = cached_tui_size.y.saturating_add(VERTICAL_PADDING.into());

                    // Find the distance between window edge and center
                    let half_pix_w = {
                        let cell_pix_w = u32::from(menu.sizes.font_size().x);
                        let cell_w = cached_tui_size.x + HORIZONTAL_PADDING;
                        let pix_w = u32::from(cell_w) * cell_pix_w;
                        pix_w.div_ceil(2)
                    };

                    // Clamp position such that we fit. Note that this does not guarantee
                    // that there is enough space for the entire width.
                    let x = location.x.clamp(
                        half_pix_w, //
                        monitor.width.saturating_sub(half_pix_w),
                    );

                    // The left margin should be such that half the space is between
                    // left margin and x. Use saturating_sub so that the left
                    // margin becomes zero if the width would reach outside the screen.
                    let mleft = x.saturating_sub(half_pix_w);

                    // The right margin is calculated the same way, but starting from the right edge.
                    let mright = (monitor.width - x).saturating_sub(half_pix_w);

                    // The font size (on which cell->pixel conversion is based) and the monitor's
                    // size are in physical pixels. This makes sense because different monitors can
                    // have different scales, and the application should not be affected by that
                    // (this is not x11 after all).
                    // However, panels are bound to a monitor and the margins are in scaled pixels,
                    // so we have to make this correction.
                    let margin_left = (f64::from(mleft) / scale) as u32;
                    let margin_right = (f64::from(mright) / scale) as u32;

                    menu.term_upd_tx
                        .send(TermUpdate::RemoteControl(vec![
                            "resize-os-window".into(),
                            "--incremental".into(),
                            "--action=os-panel".into(),
                            format!("margin-left={margin_left}").into(),
                            format!("margin-right={margin_right}").into(),
                            format!("lines={lines}").into(),
                        ]))
                        .ok_or_log();
                }

                // TODO: be smarter about when to run this
                let action = if show_menu.is_some() { "show" } else { "hide" };
                menu.term_upd_tx
                    .send(TermUpdate::RemoteControl(vec![
                        "resize-os-window".into(),
                        format!("--action={}", action).into(),
                    ]))
                    .ok_or_log();
            }

            if let Some(ShowMenu {
                ref mut tui,
                rendered: ref mut rendered @ false,
                cached_tui_size,
                ref sizing,
                ..
            }) = show_menu
            {
                *rendered = true;

                let mut buf = Vec::new();

                // NOTE: The terminal might not be done resizing at this point,
                // which would cause issues if passing the terminal's size here.
                // Passing the tui's desired size sidesteps this because kitty
                // will rerender it correctly once the resize is done.
                if let Some(layout) = tui::render(
                    tui,
                    tui::Area {
                        size: cached_tui_size,
                        pos: tui::Vec2 {
                            x: HORIZONTAL_PADDING / 2,
                            y: 0,
                        },
                    },
                    &mut buf,
                    sizing,
                )
                .context("Failed to draw menu")
                .ok_or_log()
                {
                    menu.layout = Some(layout);
                    menu.term_upd_tx.send(TermUpdate::Print(buf)).ok_or_log();
                    menu.term_upd_tx.send(TermUpdate::Flush).ok_or_log();
                }
            }
        }
    };

    loop {
        try_run()
            .await
            .with_context(|| {
                format!(
                    "Failed to run panels for monitor {}. Retrying in 30s",
                    monitor.name
                )
            })
            .ok_or_log();
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

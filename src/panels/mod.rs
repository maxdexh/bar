pub(crate) mod proc;

use proc::{TermEvent, TermUpdate};

use std::{collections::HashMap, ffi::OsString, sync::Arc, time::Duration};

use anyhow::Context;
use futures::StreamExt;
use tokio::task::JoinSet;
use tokio_util::{sync::CancellationToken, time::FutureExt as _};

use crate::{
    monitors::MonitorInfo,
    tui,
    utils::{
        CancelDropGuard, ReloadTx, ResultExt, UnbRx, UnbTx, WatchRx, run_or_retry, unb_chan,
        watch_chan,
    },
};

// FIXME: Add to args of run_manager
const EDGE: &str = "top";

/// Adds an extra line and centers the content of the menu with padding of half a cell.
const VERTICAL_PADDING: bool = true;
const HORIZONTAL_PADDING: u16 = 4;

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

pub async fn run_manager(bar_rx: WatchRx<Vec<WatchRx<BarTuiElem>>>, mut reload_tx: ReloadTx) {
    let mut monitors = HashMap::new();

    let mut monitor_rx = crate::monitors::connect();

    while let Some(ev) = monitor_rx.next().await {
        for monitor in ev.removed() {
            monitors.remove(monitor);
        }
        for monitor in ev.added_or_changed() {
            let cancel = CancellationToken::new();
            tokio::spawn(run_monitor(RunMonitorArgs {
                monitor: monitor.clone(),
                cancel_monitor: cancel.clone(),
                bar_rx: bar_rx.clone(),
                reload_tx: reload_tx.clone(),
            }));
            monitors.insert(monitor.name.clone(), CancelDropGuard::from(cancel));
        }
        reload_tx.reload();
    }
}

#[derive(Clone)]
struct RunMonitorArgs {
    monitor: MonitorInfo,
    cancel_monitor: CancellationToken,
    bar_rx: WatchRx<Vec<WatchRx<BarTuiElem>>>,
    reload_tx: ReloadTx,
}

async fn run_monitor(args: RunMonitorArgs) {
    let monitor = args.monitor.name.clone();
    run_or_retry(
        try_run_monitor,
        args,
        |it| it.with_context(|| format!("Failed to run panels for monitor {monitor}")),
        Duration::from_secs(10),
        None,
    )
    .await;
    log::debug!("Exiting panel manager for monitor {monitor:?}");
}

// FIXME: This function has like half the app logic in it.
async fn try_run_monitor(
    RunMonitorArgs {
        monitor,
        cancel_monitor,
        bar_rx,
        reload_tx,
    }: &mut RunMonitorArgs,
) -> anyhow::Result<()> {
    log::debug!("Starting panel manager for monitor {monitor:?}");

    let _auto_cancel = CancelDropGuard::from(cancel_monitor.clone());

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
    }

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

    let bar_fut = init_term(
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
    );

    let menu_fut = async {
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
    };

    let (mut bar, (mut menu, mut watcher_stream)) = async { tokio::try_join!(bar_fut, menu_fut) }
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
            let current_bar = bar_rx.borrow_and_update().clone();
            let mut listeners = JoinSet::new();

            bar_tui_tx.send_if_modified(|tui| {
                *tui = std::iter::repeat_n(BarTuiElem::Hide, current_bar.len()).collect();
                false
            });

            let (bar_changed_tx, mut bar_changed_rx) = unb_chan();
            for (i, mut rx) in current_bar.into_iter().enumerate() {
                rx.mark_changed();
                let bar_changed_tx = bar_changed_tx.clone();
                listeners.spawn(async move {
                    loop {
                        bar_changed_tx
                            .send((i, rx.borrow_and_update().clone()))
                            .ok_or_debug();

                        if rx.changed().await.is_err() {
                            log::trace!("Freezing bar part {i} since its tui channel was closed");
                            break; // stop listening
                        }
                    }
                });
            }

            loop {
                tokio::select! {
                    res = bar_rx.changed() => {
                        res?;
                        break; // restart all listeners
                    }
                    Some((i, elem)) = bar_changed_rx.next() => {
                        // Bundle together changes in 50ms windows
                        tokio::time::sleep(Duration::from_millis(50)).await;
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
        kind: MenuKind,
        pix_location: tui::Vec2<u32>,
        cached_tui_size: tui::Vec2<u16>,
        sizing: tui::SizingArgs,
        tui: tui::Elem,
    }
    let mut show_menu = None::<ShowMenu>;
    let mut show_bar = Some(tui::Elem::empty());
    loop {
        let mut rerender_menu = false;
        let mut rerender_bar = false;

        let upd = tokio::select! {
            Some(ev) = bar.term_ev_rx.next() => Upd::Term(TermKind::Bar, ev),
            Some(ev) = menu.term_ev_rx.next() => Upd::Term(TermKind::Menu, ev),
            Some(upd) = intern_upd_rx.next() => upd,
            Ok(()) = bar_tui_rx.changed() => Upd::BarTui,
            Some(res) = subtasks.join_next() => match res {
                Err(join_err) => {
                    if join_err.is_cancelled() {
                        return Ok(());
                    }
                    return Err(join_err).context("Failure joining instance task");
                },
                Ok(Err(task_err)) => return Err(task_err).context("Failure running instance task"),
            },
            () = cancel.cancelled() => return Ok(()),
        };
        match upd {
            Upd::BarTui => {
                if let Some(bar) = &mut show_bar {
                    let tui = gather_bar_tui(&bar_tui_rx.borrow_and_update(), monitor);
                    *bar = tui;
                    rerender_bar = true;
                }
            }
            Upd::Term(term_kind, ev) => match ev {
                TermEvent::Crossterm(ev) => match ev {
                    crossterm::event::Event::Mouse(ev) => {
                        let Some(layout) = (match term_kind {
                            TermKind::Menu => menu.layout.as_mut(),
                            TermKind::Bar => bar.layout.as_mut(),
                        }) else {
                            continue;
                        };
                        let mut hide_menu = false;
                        match (
                            layout.interpret_mouse_event(ev, bar.sizes.font_size()),
                            term_kind,
                        ) {
                            (tui::MouseEventResult::Ignore, _) => continue,
                            (
                                res @ (tui::MouseEventResult::HoverChanged
                                | tui::MouseEventResult::HoverEmpty
                                | tui::MouseEventResult::HoverTooltip { .. }),
                                TermKind::Menu,
                            ) => {
                                if matches!(res, tui::MouseEventResult::HoverTooltip { .. }) {
                                    log::warn!("Ignoring tooltip, which is unsupported on menu");
                                }
                                rerender_menu = true;
                            }
                            (tui::MouseEventResult::HoverChanged, TermKind::Bar) => {
                                log::warn!("Hover without tooltip is unsupported on bar");
                            }
                            (tui::MouseEventResult::HoverEmpty, TermKind::Bar) => {
                                if show_menu
                                    .as_ref()
                                    .is_some_and(|it| matches!(it.kind, MenuKind::Tooltip))
                                {
                                    hide_menu = true;
                                }
                                rerender_bar = true;
                            }
                            (
                                tui::MouseEventResult::Interact {
                                    pix_location,
                                    interact,
                                    tooltip,
                                },
                                _,
                            ) => {
                                if let Some((cb, args)) = interact
                                    && let Some(tui) = cb.call(args)
                                {
                                    let sizing = tui::SizingArgs {
                                        font_size: menu.sizes.font_size(),
                                    };
                                    show_menu = Some(ShowMenu {
                                        cached_tui_size: tui::calc_min_size(&tui, &sizing),
                                        sizing,
                                        tui,
                                        kind: MenuKind::Context,
                                        pix_location,
                                    });
                                    rerender_menu = true;
                                } else if let Some((tt, args)) = tooltip
                                    && let Some(tui) = tt.call(args)
                                {
                                    let sizing = tui::SizingArgs {
                                        font_size: menu.sizes.font_size(),
                                    };
                                    show_menu = Some(ShowMenu {
                                        cached_tui_size: tui::calc_min_size(&tui, &sizing),
                                        sizing,
                                        tui,
                                        kind: MenuKind::Context,
                                        pix_location,
                                    });
                                    rerender_menu = true;
                                } else if matches!(term_kind, TermKind::Bar) {
                                    hide_menu = true;
                                }
                            }
                            (tui::MouseEventResult::InteractEmpty, TermKind::Bar) => {
                                hide_menu = true;
                            }
                            (tui::MouseEventResult::InteractEmpty, TermKind::Menu) => {
                                continue;
                            }
                            (
                                tui::MouseEventResult::HoverTooltip {
                                    pix_location,
                                    tooltip,
                                    args,
                                },
                                TermKind::Bar,
                            ) => {
                                if show_menu
                                    .as_ref()
                                    .is_some_and(|it| matches!(it.kind, MenuKind::Context))
                                {
                                    log::debug!("Not replacing context menu with tooltip");
                                    continue;
                                }
                                let Some(tui) = tooltip.call(args) else {
                                    continue;
                                };
                                let sizing = tui::SizingArgs {
                                    font_size: menu.sizes.font_size(),
                                };
                                show_menu = Some(ShowMenu {
                                    cached_tui_size: tui::calc_min_size(&tui, &sizing),
                                    sizing,
                                    tui,
                                    kind: MenuKind::Tooltip,
                                    pix_location,
                                });
                                rerender_menu = true;
                            }
                        }
                        if hide_menu && show_menu.is_some() {
                            show_menu = None;
                            rerender_menu = true;
                        }
                    }
                    _ => {
                        //
                    }
                },
                // FIXME: Rerender on font size change
                TermEvent::Sizes(sizes) => match term_kind {
                    TermKind::Bar => {
                        bar.sizes = sizes;
                        reload_tx.reload();
                    }
                    TermKind::Menu => {
                        menu.sizes = sizes;
                    }
                },
            },
            Upd::MenuWatcherHide => {
                show_menu = None;
                if let Some(layout) = &mut bar.layout
                    && layout.reset_hover()
                {
                    rerender_bar = true;
                }
            }
        }

        if rerender_menu {
            if show_menu.is_none()
                && let Some(layout) = &mut bar.layout
                && layout.reset_hover()
            {
                rerender_bar = true;
            }

            if let Some(ShowMenu {
                pix_location: location,
                cached_tui_size,
                ref tui,
                ref sizing,
                kind: _,
            }) = show_menu
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
                    menu.layout.as_ref(),
                )
                .context("Failed to draw menu")
                .ok_or_log()
                {
                    menu.layout = Some(layout);
                    menu.term_upd_tx.send(TermUpdate::Print(buf)).ok_or_log();
                    menu.term_upd_tx.send(TermUpdate::Flush).ok_or_log();
                }
            }

            let action = if show_menu.is_some() { "show" } else { "hide" };
            menu.term_upd_tx
                .send(TermUpdate::RemoteControl(vec![
                    "resize-os-window".into(),
                    format!("--action={}", action).into(),
                ]))
                .ok_or_debug();
        }

        if rerender_bar && let Some(tui) = &show_bar {
            let mut buf = Vec::new();
            let Some(layout) = tui::render(
                tui,
                tui::Area {
                    size: bar.sizes.cell_size,
                    pos: Default::default(),
                },
                &mut buf,
                &tui::SizingArgs {
                    font_size: bar.sizes.font_size(),
                },
                bar.layout.as_ref(),
            )
            .context("Failed to render bar")
            .ok_or_log() else {
                continue;
            };
            bar.layout = Some(layout);

            bar.term_upd_tx.send(TermUpdate::Print(buf)).ok_or_debug();
            bar.term_upd_tx.send(TermUpdate::Flush).ok_or_debug();
        }
    }
}

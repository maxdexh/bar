mod monitors;
pub(crate) use bar_common::*;

use bar_proc_mgr::{TermEvent, TermUpdate};
use tempfile::TempDir;

use std::{collections::HashMap, ffi::OsString, sync::Arc, time::Duration};

use anyhow::Context;
use futures::StreamExt;
use tokio::task::JoinSet;
use tokio_util::{sync::CancellationToken, time::FutureExt as _};

use crate::{
    monitors::MonitorInfo,
    tui,
    tui::MenuKind,
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

pub struct BarTuiState {
    // FIXME: Use Option<Elem> to hide
    pub by_monitor: HashMap<Arc<str>, tui::Elem>,
    pub fallback: tui::Elem,
}
impl Default for BarTuiState {
    fn default() -> Self {
        Self {
            by_monitor: Default::default(),
            fallback: tui::Elem::empty(),
        }
    }
}

pub async fn run_controller(tui_rx: WatchRx<BarTuiState>, mut reload_tx: ReloadTx) {
    let mut monitors_auto_cancel = HashMap::new();

    let mut monitor_rx = crate::monitors::connect();

    while let Some(ev) = monitor_rx.next().await {
        for monitor in ev.removed() {
            drop(monitors_auto_cancel.remove(monitor));
        }
        for monitor in ev.added_or_changed() {
            let cancel = CancellationToken::new();
            tokio::spawn(run_monitor(RunMonitorArgs {
                monitor: monitor.clone(),
                cancel_monitor: cancel.clone(),
                bar_rx: tui_rx.clone(),
                reload_tx: reload_tx.clone(),
            }));
            monitors_auto_cancel.insert(monitor.name.clone(), CancelDropGuard::from(cancel));
        }
        reload_tx.reload();
    }
}

#[derive(Clone)]
struct RunMonitorArgs {
    monitor: MonitorInfo,
    cancel_monitor: CancellationToken,
    bar_rx: WatchRx<BarTuiState>,
    reload_tx: ReloadTx,
}

async fn run_monitor(args: RunMonitorArgs) {
    let monitor = args.monitor.name.clone();
    let _auto_cancel = CancelDropGuard::from(args.cancel_monitor.clone());

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

struct Term {
    term_ev_rx: UnbRx<TermEvent>,
    term_upd_tx: UnbTx<TermUpdate>,
    sizes: tui::Sizes,
    layout: Option<tui::RenderedLayout>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TermKind {
    Menu,
    Bar,
}
enum Upd {
    BarTui,
    MenuWatcherHide,
    Term(TermKind, TermEvent),
}

struct StartedMonitorEnv {
    bar: Term,
    menu: Term,
    intern_upd_rx: UnbRx<Upd>,
    bar_tui_rx: WatchRx<tui::Elem>,
}

async fn try_run_monitor(args: &mut RunMonitorArgs) -> anyhow::Result<()> {
    log::debug!("Starting panel manager for monitor {:?}", args.monitor);

    let mut required_tasks = JoinSet::<anyhow::Result<std::convert::Infallible>>::new();
    let cancel = args.cancel_monitor.child_token();
    let _auto_cancel = CancelDropGuard::from(cancel.clone());
    let env = try_init_monitor(&args.monitor, &args.bar_rx, &mut required_tasks, &cancel).await?;
    required_tasks.spawn(run_monitor_mainloop(
        args.monitor.clone(),
        args.reload_tx.clone(),
        env,
    ));

    if let Some(Some(res)) = required_tasks
        .join_next()
        .with_cancellation_token(&cancel)
        .await
    {
        match res {
            Err(join_err) => {
                if join_err.is_cancelled() {
                    Ok(())
                } else {
                    Err(join_err).context("Failure joining instance task")
                }
            }
            Ok(Err(task_err)) => Err(task_err).context("Failure running instance task"),
        }
    } else {
        Ok(())
    }
}

async fn run_monitor_mainloop(
    monitor: MonitorInfo,
    mut reload_tx: ReloadTx,
    mut env: StartedMonitorEnv,
) -> anyhow::Result<std::convert::Infallible> {
    #[derive(Debug)]
    struct ShowMenu {
        kind: MenuKind,
        pix_location: tui::Vec2<u32>,
        cached_size: tui::Vec2<u16>,
        sizing: tui::SizingArgs,
        tui: tui::Elem,
    }
    let mut show_menu = None::<ShowMenu>;
    let mut show_bar = Some(tui::Elem::empty());
    loop {
        let mut rerender_menu = false;
        let mut rerender_bar = false;

        let upd = tokio::select! {
            Some(ev) = env.bar.term_ev_rx.next() => Upd::Term(TermKind::Bar, ev),
            Some(ev) = env.menu.term_ev_rx.next() => Upd::Term(TermKind::Menu, ev),
            Some(upd) = env.intern_upd_rx.next() => upd,
            Ok(()) = env.bar_tui_rx.changed() => Upd::BarTui,
        };
        match upd {
            Upd::BarTui => {
                if let Some(bar) = &mut show_bar {
                    *bar = env.bar_tui_rx.borrow_and_update().clone();
                    rerender_bar = true;
                }
            }
            Upd::Term(term_kind, ev) => match ev {
                TermEvent::Crossterm(ev) => match ev {
                    crossterm::event::Event::Mouse(ev) => {
                        let Some(layout) = (match term_kind {
                            TermKind::Menu => env.menu.layout.as_mut(),
                            TermKind::Bar => env.bar.layout.as_mut(),
                        }) else {
                            continue;
                        };

                        let tui::MouseEventResult {
                            interact,
                            callback,
                            empty,
                            changed,
                            rerender,
                            pix_location,
                        } = layout.interpret_mouse_event(ev, env.bar.sizes.font_size());
                        let is_hover = interact.kind == tui::InteractKind::Hover;

                        if term_kind == TermKind::Menu
                            && let Some(menu) = &show_menu
                            && menu.kind == MenuKind::Tooltip
                        {
                            show_menu = None;
                            rerender_menu = true;
                        }

                        if rerender {
                            match term_kind {
                                TermKind::Menu => rerender_menu = true,
                                TermKind::Bar => rerender_bar = true,
                            }
                        }

                        if changed || !is_hover {
                            if empty
                                && term_kind == TermKind::Bar
                                && let Some(menu) = &show_menu
                                && (!is_hover || menu.kind == MenuKind::Tooltip)
                            {
                                show_menu = None;
                                rerender_menu = true;
                            }

                            if let Some(callback) = callback
                                && let Some(tui::OpenMenu { tui, menu_kind }) =
                                    callback.call(interact)
                            {
                                let sizing = tui::SizingArgs {
                                    font_size: env.menu.sizes.font_size(),
                                };
                                show_menu = Some(ShowMenu {
                                    cached_size: tui::calc_min_size(&tui, &sizing),
                                    sizing,
                                    tui,
                                    kind: menu_kind,
                                    pix_location,
                                });
                                rerender_menu = true;
                            }
                        }
                    }
                    _ => {
                        //
                    }
                },
                // FIXME: Rerender on font size change
                TermEvent::Sizes(sizes) => match term_kind {
                    TermKind::Bar => {
                        env.bar.sizes = sizes;
                        reload_tx.reload();
                    }
                    TermKind::Menu => {
                        env.menu.sizes = sizes;
                    }
                },
            },
            Upd::MenuWatcherHide => {
                show_menu = None;
                if let Some(layout) = &mut env.bar.layout
                    && layout.ext_focus_loss()
                {
                    rerender_bar = true;
                }
            }
        }

        if rerender_menu {
            if show_menu.is_none()
                && let Some(layout) = &mut env.bar.layout
                && layout.ext_focus_loss()
            {
                rerender_bar = true;
            }

            if let Some(ShowMenu {
                pix_location: location,
                cached_size: cached_tui_size,
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
                    let cell_pix_w = u32::from(env.menu.sizes.font_size().x);
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

                env.menu
                    .term_upd_tx
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
                    env.menu.layout.as_ref(),
                )
                .context("Failed to draw menu")
                .ok_or_log()
                {
                    env.menu.layout = Some(layout);
                    env.menu
                        .term_upd_tx
                        .send(TermUpdate::Print(buf))
                        .ok_or_log();
                    env.menu.term_upd_tx.send(TermUpdate::Flush).ok_or_log();
                }
            }

            let action = if show_menu.is_some() { "show" } else { "hide" };
            env.menu
                .term_upd_tx
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
                    size: env.bar.sizes.cell_size,
                    pos: Default::default(),
                },
                &mut buf,
                &tui::SizingArgs {
                    font_size: env.bar.sizes.font_size(),
                },
                env.bar.layout.as_ref(),
            )
            .context("Failed to render bar")
            .ok_or_log() else {
                continue;
            };
            env.bar.layout = Some(layout);

            env.bar
                .term_upd_tx
                .send(TermUpdate::Print(buf))
                .ok_or_debug();
            env.bar.term_upd_tx.send(TermUpdate::Flush).ok_or_debug();
        }
    }
}

async fn init_term(
    sock_path: std::path::PathBuf,
    log_name: String,
    extra_args: impl IntoIterator<Item = OsString>,
    extra_envs: impl IntoIterator<Item = (OsString, OsString)>,
    cancel: &CancellationToken,
) -> anyhow::Result<Term> {
    let (term_upd_tx, term_upd_rx) = unb_chan();
    let (term_ev_tx, mut term_ev_rx) = unb_chan();

    bar_proc_mgr::start_generic_panel(
        &sock_path,
        &log_name,
        term_upd_rx,
        extra_args,
        extra_envs,
        term_ev_tx,
        cancel.clone(),
    )
    .await?;

    let sizes = loop {
        match term_ev_rx.next().await {
            Some(TermEvent::Sizes(sizes)) => break sizes,
            Some(ev) => {
                log::error!("Ignoring term event {ev:?}. The first event should be _::Sizes");
            }
            None => {
                anyhow::bail!("Failure receiving initial size event from terminal (channel closed)")
            }
        }
    };

    anyhow::Ok(Term {
        sizes,
        layout: Default::default(),
        term_ev_rx,
        term_upd_tx,
    })
}

async fn try_init_monitor(
    monitor: &MonitorInfo,
    bar_rx: &WatchRx<BarTuiState>,
    required_tasks: &mut JoinSet<anyhow::Result<std::convert::Infallible>>,
    cancel: &CancellationToken,
) -> anyhow::Result<StartedMonitorEnv> {
    let mut bar_rx = bar_rx.clone();
    let monitor = monitor.clone();

    let (intern_upd_tx, intern_upd_rx) = unb_chan();

    let tmpdir = tokio::task::spawn_blocking(TempDir::new).await??;

    let bar_fut = init_term(
        tmpdir.path().join("bar-term-socket.sock"),
        format!("BAR@{}", monitor.name),
        [
            format!("--output-name={}", monitor.name).into(),
            // Allow logging to $KITTY_STDIO_FORWARDED
            "-o=forward_stdio=yes".into(),
            // Do not use the system's kitty.conf
            "--config=NONE".into(),
            // Basic look of the bar
            "-o=foreground=white".into(),
            "-o=background=black".into(),
            // location of the bar
            format!("--edge={}", EDGE).into(),
            // disable hiding the mouse
            "-o=mouse_hide_wait=0".into(),
        ],
        [],
        cancel,
    );

    let menu_fut = async {
        let watcher_py = tmpdir.path().join("menu_watcher.py");

        tokio::fs::write(&watcher_py, include_bytes!("menu_watcher.py")).await?;

        let watcher_sock_path = tmpdir.path().join("menu_watcher.sock");
        let watcher_sock = tokio::net::UnixListener::bind(&watcher_sock_path)?;

        let menu = init_term(
            tmpdir.path().join("menu-term-socket.sock"),
            format!("MENU@{}", monitor.name),
            [
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
            [("BAR_MENU_WATCHER_SOCK".into(), watcher_sock_path.into())],
            cancel,
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

    let res = async { tokio::try_join!(bar_fut, menu_fut) }
        .timeout(Duration::from_secs(10))
        .await;

    // We have connected to the sockets, there is no need to keep the files around.
    tokio::task::spawn_blocking(move || drop(tmpdir));

    let (bar, (menu, mut watcher_stream)) = res??;

    required_tasks.spawn({
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

    let (bar_tui_tx, bar_tui_rx) = watch_chan(tui::Elem::empty());
    tokio::spawn(async move {
        while let Ok(()) = bar_rx.changed().await {
            let tui = {
                let lock = bar_rx.borrow_and_update();
                lock.by_monitor
                    .get(&monitor.name)
                    .unwrap_or(&lock.fallback)
                    .clone()
            };
            bar_tui_tx.send_if_modified(|cur| {
                if cur.is_identical(&tui) {
                    return false;
                }
                *cur = tui;
                true
            });
        }
    });

    Ok(StartedMonitorEnv {
        bar,
        menu,
        intern_upd_rx,
        bar_tui_rx,
    })
}

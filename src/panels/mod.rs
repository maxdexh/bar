pub(crate) mod proc;

use proc::{TermEvent, TermUpdate};

use std::{collections::HashMap, ffi::OsString, sync::Arc, time::Duration};

use anyhow::Context;
use futures::Stream;
use tokio::task::JoinSet;
use tokio_stream::StreamExt;
use tokio_util::{sync::CancellationToken, time::FutureExt as _};

use crate::{
    modules::prelude::{
        MenuKind, ModuleAct, ModuleId, ModuleInteract, ModuleInteractPayload, ModuleUpd, OpenMenu,
    },
    monitors::{MonitorEvent, MonitorInfo},
    tui,
    utils::{
        CancelDropGuard, Emit, EmitResult, ReloadRx, ReloadTx, ResultExt, SharedEmit, UnbRx, UnbTx,
        WatchRx, WatchTx, unb_chan,
    },
};

const EDGE: &str = "top";

#[derive(Clone, Debug)]
pub struct ModuleActTxImpl {
    id: ModuleId,
    tx: UnbTx<Upd>,
}
impl Emit<ModuleAct> for ModuleActTxImpl {
    fn try_emit(&mut self, val: ModuleAct) -> EmitResult<ModuleAct> {
        let id = self.id.clone();
        self.tx
            .try_emit(Upd::Act(id, val))
            .map_err(|err| err.retype())
    }
}
pub type ModuleUpdRxImpl = tokio_stream::wrappers::UnboundedReceiverStream<ModuleUpd>;

pub enum BarMgrUpd {
    LoadModules(LoadModules),
}
pub struct BarMgrModuleArgs {
    pub act_tx: ModuleActTxImpl,
    pub upd_rx: ModuleUpdRxImpl,
    pub reload_rx: crate::utils::ReloadRx,
    pub cancel: tokio_util::sync::CancellationToken,
    // TODO: Config
}
pub struct BarMgrModuleParams {
    pub start: Box<dyn FnOnce(BarMgrModuleArgs) -> anyhow::Result<()> + Send>,
}

pub struct LoadModules {
    pub start: HashMap<ModuleId, BarMgrModuleParams>,
    pub left: Vec<ModuleId>,
    pub right: Vec<ModuleId>,
}

struct ModuleInst {
    _cancel: CancelDropGuard,
    tui: BarTuiElem,
    upd_tx: UnbTx<ModuleUpd>,
}
fn mod_inst(
    old: Option<ModuleInst>,
    reload_rx: ReloadRx,
    act_tx: ModuleActTxImpl,
) -> (ModuleInst, BarMgrModuleArgs) {
    let (upd_tx, upd_rx) = tokio::sync::mpsc::unbounded_channel();
    let cancel = CancellationToken::new();

    let tui = old.map_or_else(|| BarTuiElem::Shared(Default::default()), |it| it.tui);
    (
        ModuleInst {
            _cancel: cancel.clone().into(),
            tui,
            upd_tx,
        },
        BarMgrModuleArgs {
            cancel,
            act_tx,
            upd_rx: upd_rx.into(),
            reload_rx,
        },
    )
}

enum Upd {
    Bar(BarMgrUpd),
    Monitor(MonitorEvent),
    Act(ModuleId, ModuleAct),
    Mod(ModuleUpd), // FIXME: (ModuleId, ModuleUpd)
}

#[derive(Debug, Clone)]
enum BarTuiElem {
    Shared(Arc<tui::Elem>),
    ByMonitor(HashMap<Arc<str>, Arc<tui::Elem>>),
    Hide,
}
#[derive(Default)]
struct BarTui {
    left: Vec<BarTuiElem>,
    right: Vec<BarTuiElem>,
}
impl BarTui {
    fn to_tui(&self, monitor: &MonitorInfo) -> tui::Tui {
        let mut parts = Vec::new();
        let ap = |parts: &mut Vec<_>, elem: &_| {
            // TODO: Error on missing
            let elem = match elem {
                BarTuiElem::Shared(elem) => Some(elem.clone()),
                BarTuiElem::ByMonitor(elems) => Some(elems.get(&monitor.name).cloned()?),
                BarTuiElem::Hide => None,
            };
            parts.extend(elem.map(tui::StackItem::auto));
            Some(())
        };
        parts.push(tui::StackItem::spacing(1));
        for elem in &self.left {
            ap(&mut parts, elem);
        }
        parts.push(tui::StackItem::new(tui::Constr::Fill(1), tui::Elem::Empty));
        for elem in &self.right {
            ap(&mut parts, elem);
        }
        parts.push(tui::StackItem::spacing(1));
        tui::Tui {
            root: Box::new(tui::Stack::horizontal(parts).into()),
        }
    }
}

pub async fn run_manager(bar_upd_rx: impl Stream<Item = BarMgrUpd> + Send + 'static) {
    tokio::pin!(bar_upd_rx);

    let mut reload_tx = crate::utils::ReloadTx::new();

    #[derive(Default)]
    struct State {
        modules: HashMap<ModuleId, ModuleInst>,
        left: Vec<ModuleId>,
        right: Vec<ModuleId>,
        monitors: HashMap<Arc<str>, Monitor>,
    }
    struct Monitor {
        menu_tx: UnbTx<OpenMenu>,
        _cancel: CancelDropGuard,
    }
    let mut state = State::default();
    let bar_tui_tx = WatchTx::new(BarTui::default());

    let (mgr_upd_tx, mut mgr_upd_rx) = unb_chan();

    crate::monitors::connect(mgr_upd_tx.clone().with(Upd::Monitor));

    loop {
        // TODO: listen for monitor cancellations
        let upd = tokio::select! {
            Some(upd) = bar_upd_rx.next() => Upd::Bar(upd),
            Some(upd) = mgr_upd_rx.next() => upd,
        };
        match upd {
            Upd::Mod(ev) => {
                // FIXME: Targeted
                state
                    .modules
                    .retain(|_, module| module.upd_tx.try_emit(ev.clone()).is_ok());
            }
            Upd::Bar(BarMgrUpd::LoadModules(LoadModules { start, left, right })) => {
                state.left = left;
                state.right = right;
                let mut old = std::mem::take(&mut state.modules);
                for (id, m) in start {
                    let (inst, args) = mod_inst(
                        old.remove(&id),
                        reload_tx.subscribe(),
                        ModuleActTxImpl {
                            id: id.clone(),
                            tx: mgr_upd_tx.clone(),
                        },
                    );
                    if (m.start)(args).ok_or_log().is_none() {
                        continue;
                    }
                    state.modules.insert(id, inst);
                }
                reload_tx.reload();
            }
            Upd::Monitor(ev) => {
                for monitor in ev.removed() {
                    state.monitors.remove(monitor);
                }
                for monitor in ev.added_or_changed() {
                    let cancel = CancellationToken::new();
                    let (menu_tx, menu_rx) = unb_chan();
                    tokio::spawn(run_monitor(
                        monitor.clone(),
                        cancel.clone(),
                        bar_tui_tx.subscribe(),
                        menu_rx,
                        reload_tx.clone(),
                        mgr_upd_tx.clone().with(Upd::Mod),
                    ));
                    state.monitors.insert(
                        monitor.name.clone(),
                        Monitor {
                            menu_tx,
                            _cancel: cancel.into(),
                        },
                    );
                }
                reload_tx.reload();
            }
            Upd::Act(id, act) => {
                if let Some(module) = state
                    .modules
                    .get_mut(&id)
                    .with_context(|| format!("Unknown module id {id:?}"))
                    .ok_or_log()
                {
                    let mut rerender = false;
                    match act {
                        ModuleAct::RenderByMonitor(elems) => {
                            let elems = elems.into_iter().map(|(k, v)| (k, Arc::new(v))).collect();
                            module.tui = BarTuiElem::ByMonitor(elems);
                            rerender = true
                        }
                        ModuleAct::RenderAll(elem) => {
                            module.tui = BarTuiElem::Shared(Arc::new(elem));
                            rerender = true
                        }
                        ModuleAct::OpenMenu(open) => {
                            if let Some(mtr) = state.monitors.get_mut(&open.monitor) {
                                mtr.menu_tx.emit(open);
                            }
                        }
                        ModuleAct::HideModule => {
                            module.tui = BarTuiElem::Hide;
                        }
                    }

                    if rerender {
                        let mut tui = BarTui::default();
                        for (src, dst) in
                            [(&state.left, &mut tui.left), (&state.right, &mut tui.right)]
                        {
                            for mid in src {
                                let Some(module) = state.modules.get(mid) else {
                                    log::error!("Unknown module id {mid:?}");
                                    continue;
                                };
                                dst.push(module.tui.clone());
                            }
                        }
                        _ = bar_tui_tx.send(tui);
                    }
                }
            }
        }
    }
}

async fn run_monitor(
    monitor: MonitorInfo,
    cancel_monitor: CancellationToken,
    mut bar_tui_rx: WatchRx<BarTui>,
    menu_rx: impl Stream<Item = OpenMenu>,
    mut reload_tx: ReloadTx,
    mut mod_upd_tx: impl SharedEmit<ModuleUpd>, // FIXME: (ModuleId, ModuleUpd)
) {
    tokio::pin!(menu_rx);

    let _auto_cancel = CancelDropGuard::from(cancel_monitor.clone());

    struct Term {
        term_ev_rx: UnbRx<TermEvent>,
        term_upd_tx: UnbTx<TermUpdate>,
        sizes: tui::Sizes,
        layout: tui::RenderedLayout,
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
                let bar: Term = init_term(
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
                .await?;
                anyhow::Ok(bar)
            },
            // FIXME: Hide menu
            async {
                let menu: Term = init_term(
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

                let (s, _) = watcher_sock.accept().await?;
                anyhow::Ok((menu, s))
            },
        )
        .timeout(Duration::from_secs(10))
        .await??;

        subtasks.spawn({
            let mut upd_tx = intern_upd_tx.clone();
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

                    upd_tx.emit(parsed);
                }
            }
        });

        #[derive(Debug)]
        struct ShowMenu {
            tui: tui::Tui,
            kind: MenuKind,
            pos: tui::Vec2<u32>,
            tui_size_cache: tui::Vec2<u16>,
            rendered: bool,
        }
        let mut show_menu = None::<ShowMenu>;
        loop {
            // FIXME: Single state variable
            let mut resize_menu = false;
            let mut menu_render_ready = false;

            let menu_siz_ctx = || tui::SizingContext {
                font_size: menu.sizes.font_size(),
                div_w: None,
                div_h: None,
            };

            let upd = tokio::select! {
                Some(ev) = bar.term_ev_rx.next() => Upd::Term(TermKind::Bar, ev),
                Some(ev) = menu.term_ev_rx.next() => Upd::Term(TermKind::Menu, ev),
                Some(upd) = intern_upd_rx.next() => upd,
                Some(open) = menu_rx.next() => Upd::OpenMenu(open),
                Ok(()) = bar_tui_rx.changed() => Upd::BarTui,
                Some(res) = subtasks.join_next() => return res?,
            };
            match upd {
                Upd::BarTui => {
                    let tui = bar_tui_rx.borrow_and_update().to_tui(&monitor);
                    let mut buf = Vec::new();
                    let Some(layout) = tui::draw_to(&mut buf, |ctx| {
                        let size = bar.sizes.cell_size;
                        tui.render(
                            ctx,
                            tui::SizingContext {
                                font_size: bar.sizes.font_size(),
                                div_w: Some(size.x),
                                div_h: Some(size.y),
                            },
                            tui::Area {
                                size,
                                pos: Default::default(),
                            },
                        )
                    })
                    .context("Failed to render bar")
                    .ok_or_log() else {
                        continue;
                    };
                    bar.layout = layout;

                    bar.term_upd_tx.emit(TermUpdate::Print(buf));
                    bar.term_upd_tx.emit(TermUpdate::Flush);
                }
                Upd::Term(term_kind, ev) => match ev {
                    TermEvent::Crossterm(ev) => match ev {
                        crossterm::event::Event::Mouse(ev) => {
                            match bar.layout.interpret_mouse_event(ev, bar.sizes.font_size()) {
                                Some(tui::TuiInteract {
                                    location,
                                    payload: Some(target),
                                    kind,
                                }) => {
                                    mod_upd_tx.emit(ModuleUpd::Interact(ModuleInteract {
                                        location,
                                        payload: ModuleInteractPayload {
                                            tag: target.clone(),
                                            monitor: monitor.name.clone(),
                                        },
                                        kind: kind.clone(),
                                    }));
                                }
                                Some(tui::TuiInteract {
                                    location: _,
                                    payload: None,
                                    kind,
                                }) if matches!(term_kind, TermKind::Bar) => {
                                    let hide = match kind {
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
                                _ => (),
                            }
                        }
                        _ => {
                            //
                        }
                    },
                    TermEvent::Sizes(sizes) => {
                        match term_kind {
                            TermKind::Bar => {
                                bar.sizes = sizes;
                                reload_tx.reload();
                            }
                            TermKind::Menu => {
                                // FIXME: Check size not too small (and not way too big from
                                // external resize)
                                menu.sizes = sizes;
                                menu_render_ready = true;
                            }
                        }
                    }
                },
                Upd::OpenMenu(OpenMenu {
                    tui,
                    pos,
                    menu_kind,
                    ..
                }) => {
                    let tui = tui::Tui {
                        root: Box::new(tui),
                    };
                    if let Ok(cached_size) = tui
                        .calc_size(menu_siz_ctx())
                        .map_err(|err| log::error!("Failed to calculate tui size: {err}"))
                    {
                        show_menu = Some(ShowMenu {
                            tui_size_cache: cached_size,
                            tui,
                            kind: menu_kind,
                            pos,
                            rendered: false,
                        });
                        resize_menu = true;
                    }
                }
                Upd::MenuWatcherHide => {
                    show_menu = None;
                }
            }

            if resize_menu {
                if let Some(ShowMenu {
                    pos,
                    tui_size_cache,
                    rendered: false,
                    ..
                }) = show_menu
                {
                    let scale = (monitor.scale * 1000.0).ceil() / 1000.0;

                    // No need to wait before rendering if we have enough space
                    if tui_size_cache.x <= menu.sizes.cell_size.x
                        && tui_size_cache.y <= menu.sizes.cell_size.y
                    {
                        menu_render_ready = true;
                    }
                    if tui_size_cache != menu.sizes.cell_size {
                        // NOTE: There is no absolute positioning system, nor a way to directly specify the
                        // geometry (since this is controlled by the compositor). So we have to get creative by
                        // using the right and left margin to control both position and size of the panel.

                        // cap position at monitor's size
                        let x = std::cmp::min(pos.x, monitor.width);

                        // Find the distance between window edge and center
                        let half_pix_w = (u32::from(tui_size_cache.x)
                            * u32::from(menu.sizes.font_size().x))
                        .div_ceil(2);

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

                        menu.term_upd_tx.emit(TermUpdate::RemoteControl(vec![
                            "resize-os-window".into(),
                            "--incremental".into(),
                            "--action=os-panel".into(),
                            format!("margin-left={margin_left}").into(),
                            format!("margin-right={margin_right}").into(),
                            format!("lines={}", tui_size_cache.y).into(),
                        ]));
                    }
                }

                // TODO: be smarter about when to run this
                let action = if show_menu.is_some() { "show" } else { "hide" };
                menu.term_upd_tx.emit(TermUpdate::RemoteControl(vec![
                    "resize-os-window".into(),
                    format!("--action={}", action).into(),
                ]));
            }
            if menu_render_ready
                && let Some(ShowMenu {
                    ref mut tui,
                    rendered: ref mut rendered @ false,
                    tui_size_cache,
                    ..
                }) = show_menu
            {
                if tui_size_cache.x > menu.sizes.cell_size.x
                    || tui_size_cache.y > menu.sizes.cell_size.y
                {
                    log::warn!(
                        "Tui size {tui_size_cache:?} is too big for panel size {:?}",
                        menu.sizes.cell_size
                    );
                }

                let mut buf = Vec::new();
                match tui::draw_to(&mut buf, |ctx| {
                    let size = tui_size_cache; // Or term size
                    tui.render(
                        ctx,
                        tui::SizingContext {
                            font_size: menu.sizes.font_size(),
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
                        menu.layout = new_layout;
                        *rendered = true;
                    }
                }
                menu.term_upd_tx.emit(TermUpdate::Print(buf));
                menu.term_upd_tx.emit(TermUpdate::Flush);
            }
        }
    };

    loop {
        try_run()
            .await
            .with_context(|| {
                format!(
                    "Failed to run panels for monitor {}. Retrying in 5s",
                    monitor.name
                )
            })
            .ok_or_log();
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

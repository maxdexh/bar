use anyhow::{Context as _, anyhow, bail};
use futures::Stream;
use libpulse_binding::{
    self as pulse, context::introspect::ServerInfo, mainloop::standard::IterateResult,
    time::MicroSeconds, volume::Volume,
};

use pulse::{
    context::{
        Context, FlagSet, State,
        subscribe::{Facility, InterestMaskSet},
    },
    mainloop::standard::Mainloop,
    proplist::Proplist,
    volume::ChannelVolumes,
};
use tokio::task::JoinSet;
use tokio_stream::StreamExt as _;
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};

use std::{
    cell::RefCell,
    rc::Rc,
    sync::{Arc, atomic::AtomicBool},
};

use crate::{
    modules::prelude::*,
    tui,
    utils::{
        CancelDropGuard, Emit, ReloadRx, ReloadTx, ResultExt, SharedEmit, UnbTx, WatchRx, unb_chan,
        watch_chan,
    },
};

#[derive(Debug, Clone, Default)]
pub struct PulseState {
    pub sink: PulseDeviceState,
    pub source: PulseDeviceState,
}

#[derive(Debug, Clone, Default)]
pub struct PulseDeviceState {
    pub name: Option<Arc<str>>,
    pub volume: f64,
    pub muted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum PulseDeviceKind {
    Sink,
    #[default]
    Source,
}

#[derive(Clone, Debug)]
pub struct PulseUpdate {
    pub kind: PulseUpdateKind,
    pub target: PulseDeviceKind,
}
#[derive(Clone, Debug)]
pub enum PulseUpdateKind {
    VolumeDelta(i32),
    ToggleMute,
    ResetVolume,
}

fn handle_iterate_result(res: IterateResult) -> anyhow::Result<()> {
    match res {
        IterateResult::Success(_) => Ok(()),
        IterateResult::Quit(retval) => Err(anyhow!("PulseAudio quit with retval {retval:#?}")),
        IterateResult::Err(paerr) => Err(paerr.into()),
    }
}

fn run_blocking(
    tx: impl SharedEmit<PulseState>,
    cancel: CancellationToken,
    awaiting_reload: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let tx = Rc::new(tx);
    log::info!("Connecting to PulseAudio");

    let mut mainloop = Mainloop::new().ok_or_else(|| anyhow!("Failed to create mainloop"))?;

    let context = {
        let mut proplist = Proplist::new().ok_or_else(|| anyhow!("Failed to create proplist"))?;

        proplist
            .set_str(
                pulse::proplist::properties::APPLICATION_NAME,
                "bar-default-device-listener",
            )
            .map_err(|()| anyhow!("Failed to set application name"))?;

        Rc::new(RefCell::new(
            Context::new_with_proplist(&mainloop, "bar-default-device-listener", &proplist)
                .ok_or_else(|| anyhow!("Failed to create context"))?,
        ))
    };

    context.borrow_mut().connect(None, FlagSet::NOFLAGS, None)?;

    loop {
        match context.borrow().get_state() {
            State::Ready => break,
            State::Failed => bail!("Context failed"),
            State::Terminated => bail!("Context terminated"),
            _ => handle_iterate_result(mainloop.iterate(true))?,
        }
    }

    let state = Rc::new(RefCell::new(PulseState::default()));

    fn update_and_send(
        kind: PulseDeviceKind,
        state: Rc<RefCell<PulseState>>,
        tx: Rc<impl SharedEmit<PulseState>>,
        context: &RefCell<Context>,
    ) {
        let name = {
            let state = state.borrow();
            match kind {
                PulseDeviceKind::Sink => &state.sink.name,
                PulseDeviceKind::Source => &state.source.name,
            }
            .clone()
        };
        let Some(name) = name else {
            return;
        };

        let doit = move |volume: &ChannelVolumes, muted: bool| {
            tx.emit({
                let mut state = state.borrow_mut();
                let dstate = match kind {
                    PulseDeviceKind::Sink => &mut state.sink,
                    PulseDeviceKind::Source => &mut state.source,
                };
                dstate.volume = avg_volume_frac(volume);
                dstate.muted = muted;
                state.clone()
            });
        };

        match kind {
            PulseDeviceKind::Sink => {
                context
                    .borrow()
                    .introspect()
                    .get_sink_info_by_name(&name, move |res| {
                        if let pulse::callbacks::ListResult::Item(info) = res {
                            doit(&info.volume, info.mute)
                        }
                    });
            }
            PulseDeviceKind::Source => {
                context
                    .borrow()
                    .introspect()
                    .get_source_info_by_name(&name, move |res| {
                        if let pulse::callbacks::ListResult::Item(info) = res {
                            doit(&info.volume, info.mute)
                        }
                    });
            }
        }
    }

    let full_update = {
        let context = context.clone();
        let state = state.clone();
        let tx = tx.clone();
        move |info: &ServerInfo<'_>| {
            {
                let mut state = state.borrow_mut();
                state.sink.name = info.default_sink_name.clone().map(Into::into);
                state.source.name = info.default_source_name.clone().map(Into::into);
            }
            update_and_send(PulseDeviceKind::Sink, state.clone(), tx.clone(), &context);
            update_and_send(PulseDeviceKind::Source, state.clone(), tx.clone(), &context)
        }
    };

    context
        .borrow()
        .introspect()
        .get_server_info(full_update.clone());

    // Subscribe to server, sink and source changes
    context.borrow_mut().subscribe(
        InterestMaskSet::SERVER | InterestMaskSet::SINK | InterestMaskSet::SOURCE,
        move |succ| {
            if !succ {
                log::error!("Failed to subscribe to PulseAudio")
            }
        },
    );

    Context::set_subscribe_callback(&mut context.clone().borrow_mut(), {
        let full_update = full_update.clone();
        let context = context.clone();
        Some(Box::new(move |facility, _, _| match facility {
            Some(Facility::Server) => {
                context
                    .borrow()
                    .introspect()
                    .get_server_info(full_update.clone());
            }
            Some(Facility::Sink) => {
                update_and_send(PulseDeviceKind::Sink, state.clone(), tx.clone(), &context);
            }
            Some(Facility::Source) => {
                update_and_send(PulseDeviceKind::Source, state.clone(), tx.clone(), &context);
            }

            fac => log::warn!("Unknown facility {fac:#?}"),
        }))
    });

    loop {
        mainloop.prepare(MicroSeconds::from_millis(500))?;
        mainloop.poll()?;
        mainloop.dispatch()?;

        if awaiting_reload.swap(false, std::sync::atomic::Ordering::Relaxed) {
            context
                .borrow()
                .introspect()
                .get_server_info(full_update.clone());
        }

        if cancel.is_cancelled() {
            break;
        }
    }

    Ok(())
}

fn avg_volume_frac(vol: &ChannelVolumes) -> f64 {
    let Volume(muted) = Volume::MUTED;
    let Volume(normal) = Volume::NORMAL;

    let sum = vol.get().iter().map(|&Volume(v)| u64::from(v)).sum::<u64>();
    let avg = (sum / vol.len() as u64).saturating_sub(muted as _);
    avg as f64 / (normal - muted) as f64
}

async fn run_updater(update_rx: impl Stream<Item = PulseUpdate>) {
    tokio::pin!(update_rx);

    while let Some(PulseUpdate { kind, target }) = update_rx.next().await {
        let (device_name, set_mute_cmd, set_vol_cmd) = match target {
            PulseDeviceKind::Sink => ("@DEFAULT_SINK@", "set-sink-mute", "set-sink-volume"),
            PulseDeviceKind::Source => ("@DEFAULT_SOURCE@", "set-source-mute", "set-source-volume"),
        };
        let pactl = || {
            let mut it = tokio::process::Command::new("pactl");
            it.stderr(std::process::Stdio::piped());
            it
        };
        let output = match kind {
            // FIXME: Task pooling/buffer_unordered
            PulseUpdateKind::VolumeDelta(vol_delta) => {
                pactl()
                    .args([set_vol_cmd, device_name, &format!("{vol_delta:+}%")])
                    .output()
                    .await
            }
            PulseUpdateKind::ToggleMute => {
                pactl()
                    .args([set_mute_cmd, device_name, "toggle"])
                    .output()
                    .await
            }
            PulseUpdateKind::ResetVolume => {
                pactl()
                    .args([set_vol_cmd, device_name, "100%"])
                    .output()
                    .await
            }
        };
        match output {
            Err(err) => log::error!("Failed to run pactl: {err}"),
            Ok(std::process::Output { status, stderr, .. }) if !status.success() => log::error!(
                "pactl exited with status {status}. Stderr: {}",
                String::from_utf8_lossy(&stderr)
            ),
            _ => (),
        }
    }
}

pub struct PulseModule {
    state_rx: WatchRx<PulseState>,
    update_tx: UnbTx<PulseUpdate>,
    reload_tx: ReloadTx,
    _background: AbortOnDropHandle<()>,
}
impl PulseModule {
    async fn run_bg(
        state_tx: impl SharedEmit<PulseState>,
        update_rx: impl Stream<Item = PulseUpdate> + 'static + Send,
        mut reload_rx: ReloadRx,
    ) {
        let mut tasks = JoinSet::<()>::new();
        tasks.spawn(run_updater(update_rx));

        let awaiting_reload = Arc::new(AtomicBool::new(false));
        let auto_cancel = CancelDropGuard::new();
        {
            let awaiting_reload = awaiting_reload.clone();
            let cancel = auto_cancel.inner.clone();
            // FIXME: Rerun on failure
            std::thread::spawn(|| {
                _ = run_blocking(state_tx, cancel, awaiting_reload)
                    .context("PulseAudio client has failed")
                    .ok_or_log()
            });
        }

        tokio::spawn(async move {
            while let Some(()) = reload_rx.wait().await {
                awaiting_reload.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        });

        if let Some(res) = tasks.join_next().await {
            res.context("PulseAudio module failed").ok_or_log();
        }
    }
}

// TODO: Implement more
#[derive(Default)]
pub struct PulseConfig {
    pub device_kind: PulseDeviceKind,
    pub muted_sym: tui::Elem,
    pub unmuted_sym: tui::Elem,
}
impl Module for PulseModule {
    type Config = PulseConfig;

    fn connect() -> Self {
        let (state_tx, state_rx) = watch_chan(Default::default());
        let (update_tx, update_rx) = unb_chan();
        let reload_tx = ReloadTx::new();
        Self {
            _background: AbortOnDropHandle::new(tokio::spawn(Self::run_bg(
                state_tx,
                update_rx,
                reload_tx.subscribe(),
            ))),
            state_rx,
            update_tx,
            reload_tx,
        }
    }

    async fn run_module_instance(
        self: Arc<Self>,
        PulseConfig {
            device_kind,
            muted_sym,
            unmuted_sym,
        }: Self::Config,
        ModuleArgs {
            act_tx,
            mut reload_rx,
            ..
        }: ModuleArgs,
        _cancel: crate::utils::CancelDropGuard,
    ) {
        let mut tasks = JoinSet::new();

        let mut reload_tx = self.reload_tx.clone();
        tasks.spawn(async move { reload_tx.reload_on(&mut reload_rx).await });

        let mut state_rx = self.state_rx.clone();
        tasks.spawn(async move {
            while let Ok(()) = state_rx.changed().await {
                let pulse = state_rx.borrow_and_update();
                let &PulseDeviceState { volume, muted, .. } = match device_kind {
                    PulseDeviceKind::Sink => &pulse.sink,
                    PulseDeviceKind::Source => &pulse.source,
                };
                //payload: tui::InteractPayload {
                //    mod_inst: inst_id.clone(),
                //    tag: tui::InteractTag::new(device_kind),
                //},
                act_tx.emit(ModuleAct::RenderAll(tui::StackItem::auto(
                    tui::Elem::from(tui::Stack::horizontal([
                        tui::StackItem::auto(if muted {
                            muted_sym.clone()
                        } else {
                            unmuted_sym.clone()
                        }),
                        tui::StackItem::auto(tui::RawPrint::plain(format!(
                            "{:>3}%",
                            (volume * 100.0).round() as u32
                        ))),
                    ]))
                    .on_interact_fn({
                        let update_tx = self.update_tx.clone();
                        move |interact| {
                            update_tx.emit(PulseUpdate {
                                target: device_kind,
                                kind: match interact.kind {
                                    tui::InteractKind::Click(tui::MouseButton::Left) => {
                                        PulseUpdateKind::ToggleMute
                                    }
                                    tui::InteractKind::Click(tui::MouseButton::Right) => {
                                        PulseUpdateKind::ResetVolume
                                    }
                                    tui::InteractKind::Scroll(direction) => {
                                        PulseUpdateKind::VolumeDelta(
                                            2 * match direction {
                                                tui::Direction::Up => 1,
                                                tui::Direction::Down => -1,
                                                tui::Direction::Left => -1,
                                                tui::Direction::Right => 1,
                                            },
                                        )
                                    }
                                    _ => return,
                                },
                            })
                        }
                    }),
                )));
            }
        });

        if let Some(res) = tasks.join_next().await {
            res.context("Pulse module failed").ok_or_log();
        }
    }
}

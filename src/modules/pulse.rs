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
use tokio_util::sync::CancellationToken;

use std::{
    cell::RefCell,
    rc::Rc,
    sync::{Arc, atomic::AtomicBool},
};

use crate::{
    modules::prelude::*,
    tui,
    utils::{CancelDropGuard, Emit, ReloadRx, ResultExt, SharedEmit, unb_chan},
};

pub fn connect(
    reload_rx: ReloadRx,
) -> (impl SharedEmit<PulseUpdate>, impl Stream<Item = PulseState>) {
    let (ev_tx, ev_rx) = unb_chan();

    let cancel = CancellationToken::new();
    let cancel_pulse_arg = CancelDropGuard::from(cancel.clone());
    std::thread::spawn(|| match run_blocking(ev_tx, reload_rx, cancel_pulse_arg) {
        Ok(()) => log::warn!("PulseAudio client has quit"),
        Err(err) => log::error!("PulseAudio client has failed: {err}"),
    });
    let (up_tx, up_rx) = unb_chan();

    tokio::task::spawn(async move {
        let _cancel = CancelDropGuard::from(cancel);
        match run_updater(up_rx).await {
            Ok(()) => log::warn!("PulseAudio updater has quit"),
            Err(err) => log::error!("PulseAudio updater has failed: {err}"),
        }
    });

    (up_tx, ev_rx)
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Default)]
pub struct PulseState {
    pub sink: PulseDeviceState,
    pub source: PulseDeviceState,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Default)]
pub struct PulseDeviceState {
    pub name: Option<Arc<str>>,
    pub volume: f64,
    pub muted: bool,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum PulseDeviceKind {
    Sink,
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
    mut reload_rx: ReloadRx,
    cancel: CancelDropGuard,
) -> anyhow::Result<()> {
    log::info!("Connecting to PulseAudio");

    let awaiting_reload = Arc::new(AtomicBool::new(false));
    std::thread::spawn({
        let awaiting_reload = awaiting_reload.clone();
        move || {
            while Arc::strong_count(&awaiting_reload) > 1 {
                let Some(()) = reload_rx.blocking_wait() else {
                    break;
                };
                awaiting_reload.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
    });

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
        mut tx: impl SharedEmit<PulseState>,
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

        let mut doit = move |volume: &ChannelVolumes, muted: bool| {
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

        if cancel.inner.is_cancelled() {
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

async fn run_updater(updates: impl Stream<Item = PulseUpdate>) -> anyhow::Result<()> {
    tokio::pin!(updates);

    while let Some(PulseUpdate { kind, target }) = updates.next().await {
        let (device_name, set_mute_cmd, set_vol_cmd) = match target {
            PulseDeviceKind::Sink => ("@DEFAULT_SINK@", "set-sink-mute", "set-sink-volume"),
            PulseDeviceKind::Source => ("@DEFAULT_SOURCE@", "set-source-mute", "set-source-volume"),
        };
        let pactl = || {
            let mut it = tokio::process::Command::new("pactl");
            it.stderr(std::process::Stdio::piped());
            it
        };
        // FIXME: Can we do this with libpulse_binding instead?
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

    log::warn!("Pulse updater exited");
    Ok(())
}

// FIXME: Refactor
pub struct PulseModule;
impl Module for PulseModule {
    async fn run_module_instance(
        self: Arc<Self>,
        ModuleArgs {
            mut act_tx,
            mut upd_rx,
            reload_rx,
            ..
        }: ModuleArgs,
        _cancel: crate::utils::CancelDropGuard,
    ) {
        fn audio_item(
            kind: PulseDeviceKind,
            &PulseDeviceState { volume, muted, .. }: &PulseDeviceState,
            unmuted_sym: impl FnOnce() -> tui::StackItem,
            muted_sym: impl FnOnce() -> tui::StackItem,
        ) -> tui::StackItem {
            tui::StackItem::auto(tui::InteractElem::new(
                Arc::new(kind),
                tui::Stack::horizontal([
                    if muted { muted_sym() } else { unmuted_sym() },
                    tui::StackItem::auto(tui::Text::plain(format!(
                        "{:>3}%",
                        (volume * 100.0).round() as u32
                    ))),
                ]),
            ))
        }
        let (mut tx, rx) = connect(reload_rx);
        let mut tasks = JoinSet::new();
        tasks.spawn(async move {
            tokio::pin!(rx);
            while let Some(pulse) = rx.next().await {
                let tui = tui::Stack::horizontal([
                    audio_item(
                        PulseDeviceKind::Source,
                        &pulse.source,
                        || {
                            // There is no double-width microphone character, so we have to build or own.
                            tui::StackItem::auto(tui::Text {
                                width: 2,
                                style: Default::default(),
                                lines: [tui::TextLine {
                                    height: 1,
                                    // https://sw.kovidgoyal.net/kitty/text-sizing-protocol/
                                    // - w=2      set width to 2
                                    // - h=2      ceter the text horizontally
                                    // - n=1/d=1  use fractional scale of 1:1. kitty ignores w without this
                                    text: "\x1b]66;w=2:h=2:n=1:d=1;\x07".into(),
                                }]
                                .into(),
                            })
                        },
                        || tui::StackItem::auto(tui::Text::plain(" ")),
                    ),
                    tui::StackItem::spacing(3),
                    audio_item(
                        PulseDeviceKind::Sink,
                        &pulse.sink,
                        || tui::StackItem::auto(tui::Text::plain(" ")),
                        || tui::StackItem::auto(tui::Text::plain(" ")),
                    ),
                    tui::StackItem::spacing(3),
                ]);
                act_tx.emit(ModuleAct::RenderAll(tui.into()));
            }
        });
        tasks.spawn(async move {
            while let Some(upd) = upd_rx.next().await {
                match upd {
                    ModuleUpd::Interact(ModuleInteract {
                        location: _,
                        payload,
                        kind,
                    }) => {
                        let Some(&target) = payload.tag.downcast_ref::<PulseDeviceKind>() else {
                            continue;
                        };
                        tx.emit(PulseUpdate {
                            target,
                            kind: match kind {
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
                                _ => continue,
                            },
                        });
                    }
                }
            }
        });

        if let Some(res) = tasks.join_next().await {
            res.context("Pulse module failed").ok_or_log();
        }
    }
}

use anyhow::{anyhow, bail};
use futures::Stream;
use libpulse_binding::{
    self as pulse, context::introspect::ServerInfo, mainloop::standard::IterateResult,
    volume::Volume,
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
use tokio::sync::broadcast;
use tokio_stream::StreamExt as _;

use std::{
    cell::RefCell,
    rc::Rc,
    sync::{Arc, atomic::AtomicBool},
};

use crate::utils::{ReloadRx, broadcast_stream};

pub fn connect(
    reload_rx: ReloadRx,
) -> (
    broadcast::Sender<PulseUpdate>,
    impl Stream<Item = PulseState>,
) {
    let (ev_tx, ev_rx) = broadcast::channel(50);
    std::thread::spawn(|| match run_blocking(ev_tx, reload_rx) {
        Ok(()) => log::warn!("PulseAudio client has quit"),
        Err(err) => log::error!("PulseAudio client has failed: {err}"),
    });
    let (up_tx, up_rx) = broadcast::channel(100);

    tokio::task::spawn(async move {
        match run_updater(broadcast_stream(up_rx)).await {
            Ok(()) => log::warn!("PulseAudio updater has quit"),
            Err(err) => log::error!("PulseAudio updater has failed: {err}"),
        }
    });

    (up_tx, broadcast_stream(ev_rx))
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

fn run_blocking(tx: broadcast::Sender<PulseState>, mut reload_rx: ReloadRx) -> anyhow::Result<()> {
    log::info!("Connecting to PulseAudio");

    let awaiting_reload = Arc::new(AtomicBool::new(false));
    tokio::spawn({
        let awaiting_reload = awaiting_reload.clone();
        async move {
            while Arc::strong_count(&awaiting_reload) > 1 {
                reload_rx.wait().await;
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
        tx: broadcast::Sender<PulseState>,
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
            {
                let mut state = state.borrow_mut();
                let dstate = match kind {
                    PulseDeviceKind::Sink => &mut state.sink,
                    PulseDeviceKind::Source => &mut state.source,
                };
                dstate.volume = avg_volume_frac(volume);
                dstate.muted = muted;
            }

            if let Err(err) = tx.send(state.borrow().clone()) {
                log::warn!("Failed to send pulse update: {err}");
                // FIXME: Exit here
            }
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
        handle_iterate_result(mainloop.iterate(true))?;
        if awaiting_reload.swap(false, std::sync::atomic::Ordering::Relaxed) {
            context
                .borrow()
                .introspect()
                .get_server_info(full_update.clone());
        }
    }
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
        let output = match kind {
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

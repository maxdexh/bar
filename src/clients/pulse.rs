use anyhow::{anyhow, bail};
use futures::{FutureExt as _, Stream};
use libpulse_binding as pulse;
use libpulse_binding::context::introspect::ServerInfo;
use libpulse_binding::mainloop::standard::IterateResult;
use libpulse_binding::volume::Volume;

use pulse::context::subscribe::{Facility, InterestMaskSet};
use pulse::context::{Context, FlagSet, State};
use pulse::mainloop::standard::Mainloop;
use pulse::proplist::Proplist;
use pulse::volume::ChannelVolumes;
use tokio::sync::broadcast;
use tokio::sync::mpsc;

use std::cell::RefCell;
use std::rc::Rc;

use crate::utils::{ResultExt as _, fused_lossy_stream};

pub fn connect() -> (mpsc::Sender<PulseUpdate>, impl Stream<Item = PulseEvent>) {
    let (ev_tx, ev_rx) = broadcast::channel(50);
    tokio::task::spawn_blocking(|| match run_blocking(ev_tx) {
        Ok(()) => log::warn!("PulseAudio client has quit"),
        Err(err) => log::error!("PulseAudio client has failed: {err}"),
    });
    let (up_tx, up_rx) = mpsc::channel(50);

    tokio::task::spawn(async move {
        match run_updater(up_rx).await {
            Ok(()) => log::warn!("PulseAudio updater has quit"),
            Err(err) => log::error!("PulseAudio updater has failed: {err}"),
        }
    });

    (up_tx, fused_lossy_stream(ev_rx))
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct PulseEvent {
    pub volume: f64,
    pub muted: bool,
    pub kind: PulseDeviceKind,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum PulseDeviceKind {
    Sink,
    Source,
}

pub struct PulseUpdate {
    pub kind: PulseUpdateKind,
    pub target: PulseDeviceKind,
}
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

// FIXME: Are the callbacks here actually guaranteed to run in the right order?
// Or do we actually have to build a tower of doom?
fn run_blocking(tx: broadcast::Sender<PulseEvent>) -> anyhow::Result<()> {
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

    #[derive(Default, Debug)]
    struct Names {
        sink: Option<Rc<str>>,
        source: Option<Rc<str>>,
    }
    impl Names {
        fn set(&mut self, info: &ServerInfo) {
            self.sink = info.default_sink_name.clone().map(Into::into);
            self.source = info.default_source_name.clone().map(Into::into);
        }
    }
    let names = Rc::new(RefCell::new(Names::default()));

    fn sendit(
        kind: PulseDeviceKind,
        names: &Names,
        tx: &broadcast::Sender<PulseEvent>,
        context: &Context,
    ) {
        let Some(name) = match kind {
            PulseDeviceKind::Sink => &names.sink,
            PulseDeviceKind::Source => &names.source,
        }
        .clone() else {
            return;
        };

        let tx = tx.clone();
        let doit = move |volume: &ChannelVolumes, muted: bool| {
            tx.send(PulseEvent {
                volume: avg_volume_frac(volume),
                muted,
                kind,
            })
            .ok_or_log("Failed to send pulse update");
        };

        match kind {
            PulseDeviceKind::Sink => {
                _ = context.introspect().get_sink_info_by_name(&name, {
                    move |res| {
                        if let pulse::callbacks::ListResult::Item(info) = res {
                            doit(&info.volume, info.mute)
                        }
                    }
                })
            }
            PulseDeviceKind::Source => {
                _ = context.introspect().get_source_info_by_name(&name, {
                    move |res| {
                        if let pulse::callbacks::ListResult::Item(info) = res {
                            doit(&info.volume, info.mute)
                        }
                    }
                })
            }
        }
    }

    let full_update = |names: Rc<RefCell<Names>>, context: Rc<RefCell<_>>, tx| {
        move |info: &ServerInfo<'_>| {
            names.borrow_mut().set(info);
            sendit(
                PulseDeviceKind::Sink,
                &names.borrow(),
                &tx,
                &context.borrow(),
            );
            sendit(
                PulseDeviceKind::Source,
                &names.borrow(),
                &tx,
                &context.borrow(),
            )
        }
    };

    context.borrow().introspect().get_server_info(full_update(
        names.clone(),
        context.clone(),
        tx.clone(),
    ));

    // Subscribe to server, sink and source changes
    context.borrow_mut().subscribe(
        InterestMaskSet::SERVER | InterestMaskSet::SINK | InterestMaskSet::SOURCE,
        move |succ| {
            if !succ {
                log::error!("Failed to subscribe to PulseAudio")
            }
        },
    );

    Context::set_subscribe_callback(
        &mut context.clone().borrow_mut(),
        Some(Box::new(move |facility, _, _| match facility {
            Some(Facility::Server) => {
                context.borrow().introspect().get_server_info(full_update(
                    names.clone(),
                    context.clone(),
                    tx.clone(),
                ));
            }
            Some(Facility::Sink) => {
                sendit(
                    PulseDeviceKind::Sink,
                    &names.borrow(),
                    &tx,
                    &context.borrow(),
                );
            }
            Some(Facility::Source) => {
                sendit(
                    PulseDeviceKind::Source,
                    &names.borrow(),
                    &tx,
                    &context.borrow(),
                );
            }

            fac => log::warn!("Unknown facility {fac:#?}"),
        })),
    );

    loop {
        handle_iterate_result(mainloop.iterate(true))?;
    }
}

fn avg_volume_frac(vol: &ChannelVolumes) -> f64 {
    let Volume(muted) = Volume::MUTED;
    let Volume(normal) = Volume::NORMAL;

    let sum = vol.get().iter().map(|&Volume(v)| u64::from(v)).sum::<u64>();
    let avg = (sum / vol.len() as u64).saturating_sub(muted as _);
    avg as f64 / (normal - muted) as f64
}

async fn run_updater(mut rx: mpsc::Receiver<PulseUpdate>) -> anyhow::Result<()> {
    let mut queued = Vec::new();
    while rx.recv_many(&mut queued, 1000).await > 0 {
        #[derive(Default)]
        struct Total {
            toggle_mute: bool,
            vol_delta: i32,
            reset_vol: bool,
        }
        let mut sink_total = Total::default();
        let mut source_total = Total::default();

        for PulseUpdate { kind, target } in queued.drain(..) {
            let total = match target {
                PulseDeviceKind::Sink => &mut sink_total,
                PulseDeviceKind::Source => &mut source_total,
            };
            match kind {
                PulseUpdateKind::VolumeDelta(d) => total.vol_delta += d,
                PulseUpdateKind::ToggleMute => total.toggle_mute ^= true,
                PulseUpdateKind::ResetVolume => {
                    // Set the volume delta to zero so that we can assume
                    // that any volume change came after the reset
                    total.vol_delta = 0;
                    total.reset_vol = true;
                }
            }
        }

        for (
            Total {
                toggle_mute,
                vol_delta,
                reset_vol,
            },
            (device_name, set_mute_cmd, set_vol_cmd),
        ) in [
            (
                sink_total,
                ("@DEFAULT_SINK@", "set-sink-mute", "set-sink-volume"),
            ),
            (
                source_total,
                ("@DEFAULT_SOURCE@", "set-source-mute", "set-source-volume"),
            ),
        ] {
            let pactl = || tokio::process::Command::new("pactl");

            if reset_vol {
                pactl()
                    .args([set_vol_cmd, device_name, "100%"])
                    .output()
                    .await
                    .ok_or_log("Failed to run pactl");
            }

            futures::future::join_all(
                [
                    (vol_delta != 0).then(|| {
                        pactl()
                            .args([set_vol_cmd, device_name, &format!("{vol_delta:+}%")])
                            .output()
                    }),
                    toggle_mute
                        .then(|| pactl().args([set_mute_cmd, device_name, "toggle"]).output()),
                ]
                .into_iter()
                .flatten()
                .map(|fut| fut.map(|res| res.ok_or_log("Failed to run pactl"))),
            )
            .await;
        }
    }

    log::warn!("Pulse updater exited");
    Ok(())
}

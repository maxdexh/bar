use super::prelude::*;
use std::{sync::Arc, time::Duration};

use chrono::Timelike;
use futures::Stream;

use crate::{
    tui,
    utils::{CancelDropGuard, Emit, ReloadRx, stream_from_fn, watch_chan},
};

const MIN_SLEEP: Duration = Duration::from_millis(250);

fn send_time(tx: &mut impl Emit<String>, time: chrono::DateTime<chrono::Local>) {
    tx.emit(time.format("%H:%M %d/%m").to_string())
}

pub fn connect(mut reload_rx: ReloadRx) -> impl Stream<Item = String> {
    let (mut tx, mut rx) = watch_chan(Default::default());
    tokio::spawn(run(tx.clone()));
    tokio::spawn(async move {
        loop {
            reload_rx.wait().await;
            send_time(&mut tx, chrono::Local::now());
        }
    });
    stream_from_fn(async move || {
        rx.changed().await.ok()?;
        Some(rx.borrow_and_update().clone())
    })
}

async fn run(mut tx: impl Emit<String>) {
    let mut last_minutes = 100;
    loop {
        let now = chrono::Local::now();
        let minute = now.minute();
        if minute != last_minutes {
            send_time(&mut tx, now);
            last_minutes = minute;
        } else {
            tokio::time::sleep(
                MIN_SLEEP.max(Duration::from_millis(Into::into(500 * (60 - now.second())))),
            )
            .await;
        }
    }
}

pub struct TimeModule;
impl Module for TimeModule {
    async fn run_module_instance(
        self: Arc<Self>,
        ModuleArgs {
            mut act_tx,
            mut reload_rx,
            ..
        }: ModuleArgs,
        _cancel: CancelDropGuard,
    ) {
        let mut last_minutes = None;
        loop {
            let now = chrono::Local::now();
            let minute = now.minute();
            if last_minutes != Some(minute) {
                let tui = tui::Text::plain(now.format("%H:%M %d/%m").to_string());
                act_tx.emit(ModuleAct::RenderAll(tui.into()));

                last_minutes = Some(minute);
            } else {
                let timeout =
                    Duration::from_millis(500 * (60 - u64::from(now.second()))).max(MIN_SLEEP);
                tokio::select! {
                    _ = reload_rx.wait() => last_minutes = None,
                    _ = tokio::time::sleep(timeout) => {}
                }
            }
        }
    }
}

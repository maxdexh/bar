use std::{ops::ControlFlow, time::Duration};

use chrono::Timelike;
use futures::Stream;
use tokio::sync::broadcast;

use crate::utils::{ReloadRx, lossy_broadcast};

const MIN_SLEEP: Duration = Duration::from_millis(250);

async fn send_time(
    tx: &broadcast::Sender<String>,
    time: chrono::DateTime<chrono::Local>,
) -> ControlFlow<()> {
    if let Err(err) = tx.send(time.format("%H:%M %d/%m").to_string()) {
        log::warn!("Time channel closed: {err}");
        ControlFlow::Break(())
    } else {
        ControlFlow::Continue(())
    }
}

pub fn connect(mut reload_rx: ReloadRx) -> impl Stream<Item = String> {
    let (tx, rx) = broadcast::channel(5);
    tokio::spawn(run(tx.clone()));
    tokio::spawn(async move {
        loop {
            reload_rx.wait().await;
            if send_time(&tx, chrono::Local::now()).await.is_break() {
                break;
            }
        }
    });
    lossy_broadcast(rx)
}

async fn run(tx: broadcast::Sender<String>) {
    let mut last_minutes = 100;
    loop {
        let now = chrono::Local::now();
        let minute = now.minute();
        if minute != last_minutes {
            if send_time(&tx, now).await.is_break() {
                break;
            }
            last_minutes = minute;
        } else {
            tokio::time::sleep(MIN_SLEEP.max(tokio::time::Duration::from_millis(Into::into(
                500 * (60 - now.second()),
            ))))
            .await;
        }
    }
}

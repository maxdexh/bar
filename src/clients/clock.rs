use chrono::Timelike;
use futures::Stream;
use tokio::sync::broadcast;

use crate::utils::fused_lossy_stream;

const MIN_SLEEP: tokio::time::Duration = tokio::time::Duration::from_millis(250);

pub fn connect() -> impl Stream<Item = String> {
    let (tx, rx) = broadcast::channel(5);
    tokio::spawn(async move {
        let mut last_minutes = 100;
        loop {
            let now = chrono::Local::now();
            let minute = now.minute();
            if minute != last_minutes {
                if let Err(err) = tx.send(now.format("%H:%M %d/%m").to_string()) {
                    log::warn!("Time channel closed: {err}");
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
    });
    fused_lossy_stream(rx)
}

use futures::Stream;
use tokio::sync::broadcast;

use crate::utils::stream_from_fn;

// TODO: Debounce
// TODO: use tokio::sync::Notify
pub struct ReloadRx {
    rx: broadcast::Receiver<()>,
    //last_reload: Option<std::time::Instant>,
    //min_delay: std::time::Duration,
}
impl ReloadRx {
    #[must_use]
    pub fn blocking_wait(&mut self) -> Option<()> {
        match self.rx.blocking_recv() {
            Ok(()) | Err(broadcast::error::RecvError::Lagged(_)) => Some(()),
            _ => None,
        }
    }
    pub async fn wait(&mut self) {
        if self.try_wait().await.is_none() {
            std::future::pending().await
        }
    }
    #[must_use]
    pub async fn try_wait(&mut self) -> Option<()> {
        // if we get Err(Lagged) or Ok, then a reload request was issued.
        // FIXME: Lagged should always be accompanied by an actualy item,
        // unless we just happen to miss it because it also gets lagged
        // before we can handle it.
        // So we are basically reloading twice to avoid an unreasonable edge case.
        match self.rx.recv().await {
            Ok(()) | Err(broadcast::error::RecvError::Lagged(_)) => Some(()),
            _ => None,
        }
    }
    pub fn into_stream(mut self) -> impl Stream<Item = ()> {
        stream_from_fn(async move || self.try_wait().await)
    }
}
#[derive(Clone)]
pub struct ReloadTx {
    tx: broadcast::Sender<()>,
}
impl ReloadTx {
    pub fn new() -> Self {
        Self {
            tx: broadcast::Sender::new(1),
        }
    }
    pub fn reload(&mut self) {
        _ = self.tx.send(());
    }
    pub fn subscribe(&self) -> ReloadRx {
        ReloadRx {
            rx: self.tx.subscribe(),
        }
    }
    pub async fn reload_on(&mut self, rx: &mut ReloadRx) {
        while let Some(()) = rx.try_wait().await {
            self.reload();
        }
    }
}

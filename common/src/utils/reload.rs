use crate::utils::{WatchRx, WatchTx};

// TODO: use tokio::sync::Notify
// TODO: Consider numbering reloads to deduplicate module backend reloads
#[derive(Clone)]
pub struct ReloadRx {
    rx: WatchRx<()>,
    // FIXME: Debounce
    //last_reload: Option<std::time::Instant>,
    //min_delay: std::time::Duration,
}
impl ReloadRx {
    #[must_use]
    pub async fn wait(&mut self) -> Option<()> {
        let opt = self.rx.changed().await.ok();
        self.rx.mark_unchanged();
        opt
    }
}
#[derive(Clone)]
pub struct ReloadTx {
    tx: WatchTx<()>,
}
impl ReloadTx {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            tx: WatchTx::new(()),
        }
    }
    pub fn reload(&mut self) {
        self.tx.send_replace(());
    }
    pub fn subscribe(&self) -> ReloadRx {
        ReloadRx {
            rx: self.tx.subscribe(),
        }
    }
}

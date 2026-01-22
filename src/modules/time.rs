use super::prelude::*;
use std::{sync::Arc, time::Duration};

use chrono::Timelike;

use crate::{
    tui,
    utils::{CancelDropGuard, Emit},
};

const MIN_SLEEP: Duration = Duration::from_millis(250);

pub struct TimeModule {}
impl Module for TimeModule {
    type Config = ();

    fn connect() -> Self {
        Self {}
    }

    async fn run_module_instance(
        self: Arc<Self>,
        cfg: Self::Config,
        ModuleArgs {
            act_tx,
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
                let tui = tui::RawPrint::plain(now.format("%H:%M %d/%m").to_string());
                act_tx.emit(ModuleAct::RenderAll(tui::StackItem::auto(tui)));

                last_minutes = Some(minute);
            } else {
                let timeout =
                    Duration::from_millis(500 * (60 - u64::from(now.second()))).max(MIN_SLEEP);
                tokio::select! {
                    Some(()) = reload_rx.wait() => last_minutes = None,
                    () = tokio::time::sleep(timeout) => {}
                }
            }
        }
    }
}

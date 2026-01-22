use crate::{modules::prelude::*, tui, utils::Emit};

pub struct FixedTuiModule {}

impl Module for FixedTuiModule {
    type Config = tui::StackItem;

    fn connect() -> Self {
        Self {}
    }

    async fn run_module_instance(
        self: std::sync::Arc<Self>,
        cfg: Self::Config,
        ModuleArgs {
            act_tx,
            mut reload_rx,
            ..
        }: ModuleArgs,
        _cancel: crate::utils::CancelDropGuard,
    ) -> () {
        let item = cfg;
        loop {
            if act_tx.try_emit(ModuleAct::RenderAll(item.clone())).is_err() {
                break;
            }
            if reload_rx.wait().await.is_none() {
                break;
            }
        }
    }
}

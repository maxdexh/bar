pub mod fixed;
pub mod hypr;
pub mod ppd;
pub mod pulse;
pub mod time;
pub mod tray;
pub mod upower;

pub mod prelude {
    use crate::{tui, utils::CancelDropGuard};
    use std::sync::Arc;

    #[non_exhaustive]
    pub enum ModuleAct {
        RenderByMonitor(std::collections::HashMap<Arc<str>, tui::StackItem>),
        RenderAll(tui::StackItem),
        HideModule,
        // FIXME: Add means to update the content of a menu.
        OpenMenu(OpenMenu),
    }
    pub struct OpenMenu {
        pub monitor: Arc<str>,
        pub tui: tui::Elem,
        pub location: tui::Vec2<u32>,
        pub menu_kind: MenuKind,
    }
    #[derive(Debug, Clone, Copy)]
    pub enum MenuKind {
        Tooltip,
        Context,
    }

    pub struct ModuleArgs {
        pub act_tx: ModuleActTx,
        pub reload_rx: crate::utils::ReloadRx,
    }
    pub type ModuleActTx = crate::panels::ModuleActTxImpl;

    pub trait Module: 'static + Send + Sync {
        type Config: 'static + Send;

        // FIXME: Take menu part of ActTx and ReloadRx here as an arg
        fn connect() -> Self;

        fn run_module_instance(
            self: Arc<Self>,
            cfg: Self::Config,
            _: ModuleArgs,
            _cancel: CancelDropGuard,
        ) -> impl Future<Output = ()> + Send;
    }
}

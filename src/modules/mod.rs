pub mod hypr;
pub mod ppd;
pub mod pulse;
pub mod time;
pub mod tray;
pub mod upower;

pub mod prelude {
    use crate::{
        tui::{self, InteractGeneric},
        utils::CancelDropGuard,
    };
    use std::sync::Arc;

    #[non_exhaustive]
    pub enum ModuleAct {
        RenderByMonitor(std::collections::HashMap<Arc<str>, tui::Elem>),
        RenderAll(tui::Elem),
        HideModule,
        OpenMenu(OpenMenu),
    }
    pub struct OpenMenu {
        pub monitor: Arc<str>,
        pub tui: tui::Elem,
        pub pos: tui::Vec2<u32>,
        pub menu_kind: MenuKind,
    }
    #[derive(Debug, Clone, Copy)]
    pub enum MenuKind {
        Tooltip,
        Context,
    }
    #[derive(Debug, Clone)] // FIXME: Remove Clone
    pub enum ModuleUpd {
        Interact(ModuleInteract),
    }
    #[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
    pub struct ModuleId(Arc<str>);
    impl ModuleId {
        pub fn new(s: &str) -> Self {
            Self(s.into())
        }
    }

    pub type ModuleInteract = InteractGeneric<ModuleInteractPayload>;
    #[derive(Debug, Clone)]
    pub struct ModuleInteractPayload {
        pub tag: tui::InteractTag,
        pub monitor: Arc<str>,
    }
    pub type ModuleArgs = crate::simple_bar::ModuleArgs;
    pub type ModuleActTx = crate::panels::ModuleActTxImpl;
    pub type ModuleUpdRx = crate::panels::ModuleUpdRxImpl;
    pub trait Module: 'static + Send + Sync {
        fn run_module_instance(
            self: Arc<Self>,
            _: ModuleArgs,
            _cancel: CancelDropGuard,
        ) -> impl Future<Output = ()> + Send;
    }
    pub struct ModuleConfig {}
}

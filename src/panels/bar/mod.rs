use std::{collections::HashMap, sync::Arc};

use tokio_util::sync::CancellationToken;

use crate::{
    tui,
    utils::{DynSharedEmit, ReloadRx},
};

pub struct ModuleArgs {
    pub tx: DynSharedEmit<ModuleAct>,
    pub rx: Box<dyn futures::Stream<Item = ModuleUpd>>,
    pub reload_rx: ReloadRx,
    pub cancel: CancellationToken,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModuleId(Arc<str>);

pub enum ModuleUpd {
    Pause,
    Unpause,
    Interact(tui::TuiInteract),
}

#[non_exhaustive]
pub enum ModuleAct {
    RenderByMonitor(HashMap<Arc<str>, Arc<tui::Elem>>),
    RenderAll(Arc<tui::Elem>),
}

pub mod manager;

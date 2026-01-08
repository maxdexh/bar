use std::collections::HashMap;

use futures::Stream;

use crate::{
    panels::bar::{ModuleArgs, ModuleId},
    utils::SharedEmit,
};

//pub async fn run(
//    bar_upd_rx: impl Stream<Item = BarMgrUpd> + Send + 'static,
//    bar_ev_tx: impl SharedEmit<(BarEventInfo, BarEvent)>,
//) {
//    let mut modules = HashMap::new();
//}

pub enum BarMgrUpd {
    LoadModules([HashMap<ModuleId, Box<dyn FnOnce(ModuleArgs) -> anyhow::Result<()>>>; 2]),
}

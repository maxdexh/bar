use anyhow::Context;
use tokio::task::JoinSet;
use tokio_util::time::FutureExt;

use crate::{
    modules::{
        self,
        prelude::{Module, ModuleId},
    },
    panels::bar::{BarMgrModuleArgs, BarMgrModuleParams, BarMgrUpd},
    utils::{Emit, ResultExt, unb_chan},
};

pub struct ModuleArgs {
    pub act_tx: crate::modules::prelude::ModuleActTx,
    pub upd_rx: crate::modules::prelude::ModuleUpdRx,
    pub reload_rx: crate::utils::ReloadRx,
    _p: (),
}

fn botch_module(mid: &ModuleId, module: impl Module) -> (ModuleId, BarMgrModuleParams) {
    let start = move |args| {
        let BarMgrModuleArgs {
            act_tx,
            upd_rx,
            reload_rx,
            cancel,
        } = args;

        tokio::spawn(async move {
            module
                .run_instance(
                    ModuleArgs {
                        act_tx,
                        upd_rx,
                        reload_rx,
                        _p: (),
                    },
                    cancel.clone().into(),
                )
                .with_cancellation_token_owned(cancel)
                .await
        });

        Ok(())
    };
    let params = BarMgrModuleParams {
        start: Box::new(start),
    };

    (mid.clone(), params)
}

pub async fn main() {
    let mut tasks = JoinSet::new();
    let mut bar_upd_tx;
    {
        let bar_upd_rx;
        (bar_upd_tx, bar_upd_rx) = unb_chan();
        tasks.spawn(crate::panels::bar::run_manager(bar_upd_rx));
    }

    let hypr = ModuleId::new("hypr");
    let clock = ModuleId::new("clock");
    let pulse = ModuleId::new("pulse");
    let ppd = ModuleId::new("ppd");
    let energy = ModuleId::new("energy");
    let tray = ModuleId::new("tray");

    bar_upd_tx.emit(BarMgrUpd::LoadModules(crate::panels::bar::LoadModules {
        start: [
            botch_module(&hypr, modules::hypr::Hypr),
            botch_module(&clock, modules::time::Time),
            botch_module(&pulse, modules::pulse::Pulse),
            botch_module(&ppd, modules::ppd::PowerProfiles),
            botch_module(&energy, modules::upower::Energy),
            botch_module(&tray, modules::tray::Tray),
        ]
        .into_iter()
        .collect(),
        left: [hypr].into(),
        right: [tray, pulse, ppd, energy, clock].into(),
    }));

    if let Some(res) = tasks.join_next().await {
        res.context("Task failed").ok_or_log();
    }
}

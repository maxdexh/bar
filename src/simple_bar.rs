use std::sync::Arc;

use anyhow::Context;
use tokio::task::JoinSet;
use tokio_util::time::FutureExt;

use crate::{
    modules::{
        self,
        prelude::{Module, ModuleArgs},
    },
    panels::{self, BarMgrModuleArgs, BarMgrModuleStartArgs, BarMgrUpd},
    tui,
    utils::{Emit, ResultExt, unb_chan},
};

fn mkstart<M: Module>(module: Arc<M>, cfg: impl Into<M::Config>) -> BarMgrModuleStartArgs {
    let cfg = cfg.into();
    let start = move |args| {
        let BarMgrModuleArgs {
            inst_id,
            act_tx,
            upd_rx,
            reload_rx,
            cancel,
        } = args;

        log::trace!(
            "Starting instance {inst_id:?} of {}",
            std::any::type_name::<M>()
        );

        tokio::spawn(async move {
            module
                .run_module_instance(
                    cfg,
                    ModuleArgs {
                        inst_id,
                        act_tx,
                        upd_rx,
                        reload_rx,
                    },
                    cancel.clone().into(),
                )
                .with_cancellation_token_owned(cancel)
                .await
        });

        Ok(())
    };
    BarMgrModuleStartArgs {
        start: Box::new(start),
    }
}

struct WeakModule<M>(std::sync::Weak<M>);
impl<M: Module> WeakModule<M> {
    pub fn get(&mut self) -> Arc<M> {
        if let Some(it) = self.0.upgrade() {
            it
        } else {
            let it = Arc::new(M::connect());
            self.0 = Arc::downgrade(&it);
            it
        }
    }
}
impl<M> Default for WeakModule<M> {
    fn default() -> Self {
        Self(Default::default())
    }
}

#[derive(Default)]
struct Modules {
    hypr: WeakModule<modules::hypr::HyprModule>,
    time: WeakModule<modules::time::TimeModule>,
    pulse: WeakModule<modules::pulse::PulseModule>,
    ppd: WeakModule<modules::ppd::PpdModule>,
    energy: WeakModule<modules::upower::EnergyModule>,
    tray: WeakModule<modules::tray::TrayModule>,
    fixed: WeakModule<modules::fixed::FixedTuiModule>,
}

pub async fn main() {
    let mut tasks = JoinSet::new();
    let mut bar_upd_tx;
    {
        let bar_upd_rx;
        (bar_upd_tx, bar_upd_rx) = unb_chan();
        tasks.spawn(panels::run_manager(bar_upd_rx));
    }

    let mut ms = Modules::default();

    bar_upd_tx.emit(BarMgrUpd::LoadModules(panels::LoadModules {
        modules: [
            mkstart(ms.fixed.get(), tui::StackItem::spacing(1)),
            mkstart(ms.hypr.get(), ()),
            mkstart(
                ms.fixed.get(),
                tui::StackItem::new(tui::Constr::Fill(1), tui::Elem::Empty),
            ),
            mkstart(ms.tray.get(), ()),
            mkstart(ms.fixed.get(), tui::StackItem::spacing(3)),
            mkstart(
                ms.pulse.get(),
                modules::pulse::PulseConfig {
                    device_kind: modules::pulse::PulseDeviceKind::Source,
                    muted_sym: tui::Text::plain(" ").into(),
                    unmuted_sym: tui::Text::plain(" ").into(),
                },
            ),
            mkstart(ms.fixed.get(), tui::StackItem::spacing(3)),
            mkstart(
                ms.pulse.get(),
                modules::pulse::PulseConfig {
                    device_kind: modules::pulse::PulseDeviceKind::Sink,
                    muted_sym: tui::Text::plain(" ").into(),
                    unmuted_sym: tui::Text::centered_symbol("", 2).into(),
                },
            ),
            mkstart(ms.fixed.get(), tui::StackItem::spacing(3)),
            mkstart(
                ms.ppd.get(),
                modules::ppd::PpdConfig {
                    icons: FromIterator::<(String, tui::Elem)>::from_iter([
                        ("balanced".into(), tui::Text::plain(" ").into()),
                        (
                            "performance".into(),
                            tui::Text::centered_symbol(" ", 2).into(),
                        ),
                        ("power-saver".into(), tui::Text::plain(" ").into()),
                    ]),
                    ..Default::default()
                },
            ),
            mkstart(ms.energy.get(), ()),
            mkstart(ms.fixed.get(), tui::StackItem::spacing(3)),
            mkstart(ms.time.get(), ()),
            mkstart(ms.fixed.get(), tui::StackItem::spacing(1)),
        ]
        .into(),
    }));

    if let Some(res) = tasks.join_next().await {
        res.context("Task failed").ok_or_log();
    }
}

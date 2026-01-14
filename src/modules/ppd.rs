use std::sync::Arc;

use anyhow::Context;
use tokio::sync::Semaphore;
use tokio_stream::StreamExt as _;

use crate::{
    modules::prelude::*,
    tui,
    utils::{
        Emit, LazyTask, LazyTaskHandle, ReloadRx, ReloadTx, ResultExt, SharedEmit, WatchRx,
        await_first, watch_chan,
    },
};

mod dbus {
    use serde::{Deserialize, Serialize};
    use zbus::{
        proxy,
        zvariant::{OwnedValue, Type, Value},
    };

    #[proxy(
        interface = "org.freedesktop.UPower.PowerProfiles",
        default_service = "org.freedesktop.UPower.PowerProfiles",
        default_path = "/org/freedesktop/UPower/PowerProfiles"
    )]
    pub trait Ppd {
        #[zbus(property)]
        fn active_profile(&self) -> zbus::Result<String>;

        #[zbus(property)]
        fn set_active_profile(&self, string: String) -> zbus::Result<()>;

        #[zbus(property)]
        fn profiles(&self) -> zbus::Result<Vec<Profile>>;
    }

    #[derive(Serialize, Deserialize, Debug, Type, OwnedValue, Value, Clone)]
    #[zvariant(signature = "dict", rename_all = "PascalCase")]
    #[serde(rename_all = "PascalCase")]
    pub struct Profile {
        pub profile: String,
        pub driver: String,
        pub platform_driver: Option<String>,
        pub cpu_driver: Option<String>,
    }
}

#[derive(Clone)]
struct PpdBackend {
    cycle: Arc<Semaphore>,
    profile_rx: WatchRx<Arc<str>>,
    reload_tx: ReloadTx,
}
// FIXME: Refactor
pub struct PpdModule {
    background: LazyTask<PpdBackend>,
}
impl PpdModule {
    pub fn new() -> Self {
        Self {
            background: LazyTask::new(),
        }
    }
    async fn enter_backend(&self) -> LazyTaskHandle<PpdBackend> {
        self.background
            .enter(|| async {
                let cycle = Arc::new(Semaphore::new(0));
                let (profile_tx, profile_rx) = watch_chan(Default::default());
                let reload_tx = ReloadTx::new();
                (
                    Self::run_backend(cycle.clone(), profile_tx, reload_tx.subscribe()),
                    PpdBackend {
                        cycle,
                        profile_rx,
                        reload_tx,
                    },
                )
            })
            .await
    }
    async fn run_backend(
        cycle_rx: Arc<Semaphore>,
        mut profile_tx: impl SharedEmit<Arc<str>>,
        mut reload_rx: ReloadRx,
    ) {
        let Some(connection) = zbus::Connection::system().await.ok_or_log() else {
            return;
        };
        let Some(proxy) = dbus::PpdProxy::new(&connection).await.ok_or_log() else {
            return;
        };

        let profiles_fut = async {
            let profile_rx = proxy.receive_active_profile_changed().await;
            tokio::pin!(profile_rx);

            loop {
                tokio::select! {
                    Some(_) = profile_rx.next() => (),
                    _ = reload_rx.wait() => (),
                };

                let Some(profile) = proxy.active_profile().await.ok_or_log() else {
                    continue;
                };

                profile_tx.emit(profile.into());
            }
        };

        let cycle_fut = async {
            loop {
                let Some(perm) = cycle_rx.acquire().await.ok_or_log() else {
                    break;
                };
                perm.forget();
                let mut steps = 1;
                while let Ok(perm) = cycle_rx.try_acquire() {
                    perm.forget();
                    steps += 1;
                }

                let Some((profiles, cur)) =
                    futures::future::try_join(proxy.profiles(), proxy.active_profile())
                        .await
                        .context("Failed to get ppd profiles")
                        .ok_or_log()
                else {
                    continue;
                };

                if profiles.is_empty() {
                    log::error!("No ppd profiles found");
                    continue;
                }

                let idx = profiles
                    .iter()
                    .position(|p| p.profile.as_str() == &cur as &str)
                    .map_or(0, |i| i + steps)
                    % profiles.len();
                let profile = profiles.into_iter().nth(idx).expect("Index < Length");

                proxy
                    .set_active_profile(profile.profile)
                    .await
                    .context("Failed to set ppd profile")
                    .ok_or_log();
            }
        };

        await_first!(profiles_fut, cycle_fut);
    }
}
impl Module for PpdModule {
    async fn run_module_instance(
        self: Arc<Self>,
        ModuleArgs {
            act_tx,
            mut upd_rx,
            mut reload_rx,
            ..
        }: ModuleArgs,
        _cancel: crate::utils::CancelDropGuard,
    ) -> () {
        let handle = self.enter_backend().await;
        let PpdBackend {
            cycle: cycle_tx,
            profile_rx,
            mut reload_tx,
        } = handle.backend().clone();

        let profile_ui_fut = async {
            let mut profile_rx = profile_rx.clone();
            let mut act_tx = act_tx.clone();
            while profile_rx.changed().await.is_ok() {
                act_tx.emit(ModuleAct::RenderAll(
                    tui::InteractElem::new(
                        Arc::new(PpdInteractTag),
                        tui::Text::plain(match &profile_rx.borrow_and_update() as &str {
                            "balanced" => " ",
                            "performance" => " ", // FIXME: Center
                            "power-saver" => " ",
                            _ => "",
                        }),
                    )
                    .into(),
                ))
            }
        };

        let reload_fut = reload_tx.reload_on(&mut reload_rx);

        let interact_fut = async {
            let profile_rx = profile_rx.clone();
            let mut act_tx = act_tx.clone();
            while let Some(upd) = upd_rx.next().await {
                match upd {
                    ModuleUpd::Interact(ModuleInteract {
                        payload: ModuleInteractPayload { tag, monitor },
                        kind,
                        location,
                    }) => {
                        let Some(PpdInteractTag {}) = tag.downcast_ref() else {
                            continue;
                        };

                        match kind {
                            tui::InteractKind::Click(tui::MouseButton::Left) => {
                                cycle_tx.add_permits(1);
                            }
                            _ => act_tx.emit(ModuleAct::OpenMenu(OpenMenu {
                                monitor,
                                tui: tui::Text::plain(&profile_rx.borrow() as &str).into(),
                                pos: location,
                                menu_kind: MenuKind::Tooltip,
                            })),
                        }
                    }
                }
            }
        };

        await_first!(profile_ui_fut, reload_fut, interact_fut);
    }
}
#[derive(Debug)]
struct PpdInteractTag;

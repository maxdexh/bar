use std::{collections::HashMap, sync::Arc};

use anyhow::Context;
use tokio::sync::Semaphore;
use tokio_stream::StreamExt as _;
use tokio_util::task::AbortOnDropHandle;

use crate::{
    modules::prelude::*,
    tui,
    utils::{Emit, ReloadRx, ReloadTx, ResultExt, SharedEmit, WatchRx, watch_chan},
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

pub struct PpdModule {
    cycle: Arc<Semaphore>,
    profile_rx: WatchRx<Option<Arc<str>>>,
    reload_tx: ReloadTx,
    _background: AbortOnDropHandle<()>,
}
impl PpdModule {
    async fn run_bg(
        cycle_rx: Arc<Semaphore>,
        profile_tx: impl SharedEmit<Option<Arc<str>>>,
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
                    Some(()) = reload_rx.wait() => (),
                };

                let profile = proxy.active_profile().await.ok_or_log();
                profile_tx.emit(profile.map(Into::into));
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

        tokio::select! {
            () = profiles_fut => (),
            () = cycle_fut => (),
        }
    }
}

#[derive(Default)]
pub struct PpdConfig {
    pub icons: HashMap<String, tui::Elem>,
    pub fallback_icon: Option<tui::Elem>,
}

impl Module for PpdModule {
    type Config = PpdConfig;

    fn connect() -> Self {
        let cycle = Arc::new(Semaphore::new(0));
        let (profile_tx, profile_rx) = watch_chan(Default::default());
        let reload_tx = ReloadTx::new();
        Self {
            _background: AbortOnDropHandle::new(tokio::spawn(Self::run_bg(
                cycle.clone(),
                profile_tx,
                reload_tx.subscribe(),
            ))),
            cycle,
            profile_rx,
            reload_tx,
        }
    }

    async fn run_module_instance(
        self: Arc<Self>,
        PpdConfig {
            icons,
            fallback_icon,
        }: Self::Config,
        ModuleArgs {
            act_tx,
            mut reload_rx,
            ..
        }: ModuleArgs,
        _cancel: crate::utils::CancelDropGuard,
    ) -> () {
        let profile_ui_fut = async {
            let mut profile_rx = self.profile_rx.clone();
            while profile_rx.changed().await.is_ok() {
                let profile = profile_rx.borrow_and_update();

                let icon = profile
                    .as_deref()
                    .and_then(|it| icons.get(it))
                    .or(fallback_icon.as_ref())
                    .cloned();

                act_tx.emit(match icon {
                    Some(elem) => {
                        ModuleAct::RenderAll(tui::StackItem::auto(elem.on_interact_fn({
                            let act_tx = act_tx.clone();
                            let profile_rx = self.profile_rx.clone();
                            let cycle = self.cycle.clone();
                            move |interact| {
                                let profile = profile_rx.borrow().clone();
                                if let Some(profile) = profile {
                                    match interact.kind {
                                        tui::InteractKind::Click(tui::MouseButton::Left) => {
                                            cycle.add_permits(1);
                                        }
                                        tui::InteractKind::Hover => {
                                            act_tx.emit(ModuleAct::OpenMenu(OpenMenu {
                                                monitor: interact.monitor,
                                                tui: tui::PlainLines::new(&profile).into(),
                                                location: interact.location,
                                                menu_kind: MenuKind::Tooltip,
                                            }))
                                        }
                                        _ => (),
                                    }
                                } else {
                                    act_tx.emit(ModuleAct::HideModule);
                                }
                            }
                        })))
                    }
                    None => ModuleAct::HideModule,
                })
            }
        };

        let mut reload_tx = self.reload_tx.clone();
        let reload_fut = reload_tx.reload_on(&mut reload_rx);

        tokio::select! {
            () = profile_ui_fut => (),
            () = reload_fut => (),
        }
    }
}

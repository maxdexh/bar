use std::sync::Arc;

use anyhow::{Context, Result};
use futures::Stream;
use tokio::task::JoinSet;
use tokio_stream::StreamExt as _;

use crate::{
    modules::prelude::*,
    tui,
    utils::{Emit, ReloadRx, ResultExt, SharedEmit, WatchRx, unb_chan, watch_chan},
};

pub struct CycleProfile;
pub fn connect(
    reload_rx: ReloadRx,
) -> (impl SharedEmit<CycleProfile>, impl Stream<Item = Arc<str>>) {
    let (switch_tx, switch_rx) = unb_chan();
    let (profile_tx, profile_rx) = unb_chan();

    tokio::spawn(async move {
        match run(profile_tx, switch_rx, reload_rx).await {
            Err(err) => log::error!("Failed to connect to ppd: {err}"),
            Ok(()) => log::warn!("Ppd client exited"),
        }
    });

    (switch_tx, profile_rx)
}

async fn run(
    mut profile_tx: impl SharedEmit<Arc<str>>,
    switch_rx: impl Stream<Item = CycleProfile>,
    reload_rx: ReloadRx,
) -> Result<()> {
    let connection = zbus::Connection::system().await?;
    let proxy = dbus::PpdProxy::new(&connection).await?;

    enum Upd {
        ProfileChanged(Arc<str>),
        CycleProfile(CycleProfile),
        Reload(()),
    }
    let active_profiles = futures::StreamExt::filter_map(
        proxy.receive_active_profile_changed().await,
        |opt| async move {
            opt.get()
                .await
                .map_err(|err| log::error!("On active profile change: {err}"))
                .ok()
                .map(|it| Upd::ProfileChanged(it.into()))
        },
    );
    let updates = active_profiles
        .merge(switch_rx.map(Upd::CycleProfile))
        .merge(reload_rx.into_stream().map(Upd::Reload));
    tokio::pin!(updates);

    let mut cur_profile = Arc::<str>::from(proxy.active_profile().await?);
    while let Some(update) = updates.next().await {
        match update {
            Upd::ProfileChanged(profile) => {
                cur_profile = profile;

                profile_tx.emit(cur_profile.clone());
            }
            Upd::Reload(()) => {
                profile_tx.emit(cur_profile.clone());
            }
            Upd::CycleProfile(CycleProfile) => {
                let Ok(profiles) = proxy
                    .profiles()
                    .await
                    .map_err(|err| log::error!("Failed to get profiles: {err}"))
                else {
                    continue;
                };
                if profiles.is_empty() {
                    log::error!("Somehow got no ppd profiles. Ignoring request.");
                    continue;
                }
                let idx = profiles
                    .iter()
                    .position(|p| p.profile.as_str() == &cur_profile as &str)
                    .map_or(0, |i| i + 1)
                    % profiles.len();

                if let Err(err) = proxy
                    .set_active_profile(profiles.into_iter().nth(idx).unwrap().profile)
                    .await
                {
                    log::error!("Failed to set profile: {err}");
                }
            }
        }
    }
    Ok(())
}

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
fn connect2(reload_rx: ReloadRx) -> (impl SharedEmit<CycleProfile>, WatchRx<Arc<str>>) {
    let (switch_tx, switch_rx) = unb_chan();
    let (profile_tx, profile_rx) = watch_chan(Default::default());

    tokio::spawn(async move {
        match run(profile_tx, switch_rx, reload_rx).await {
            Err(err) => log::error!("Failed to connect to ppd: {err}"),
            Ok(()) => log::warn!("Ppd client exited"),
        }
    });

    (switch_tx, profile_rx)
}

// FIXME: Refactor
#[derive(Debug)]
pub struct PowerProfiles;
impl Module for PowerProfiles {
    async fn run_instance(
        &self,
        ModuleArgs {
            mut act_tx,
            mut upd_rx,
            reload_rx,
            ..
        }: ModuleArgs,
        _cancel: crate::utils::CancelDropGuard,
    ) -> () {
        let (mut cycle_tx, mut profile_rx) = connect2(reload_rx);

        let mut tasks = JoinSet::new();
        tasks.spawn(async move {
            while profile_rx.changed().await.is_ok() {
                let ppd_symbol = match &profile_rx.borrow_and_update() as &str {
                    "balanced" => " ",
                    "performance" => " ",
                    "power-saver" => " ",
                    _ => "",
                };

                let tui =
                    tui::InteractElem::new(Arc::new(PowerProfiles), tui::Text::plain(ppd_symbol))
                        .into();

                act_tx.emit(ModuleAct::RenderAll(tui))
            }
        });

        tasks.spawn(async move {
            while let Some(upd) = upd_rx.next().await {
                match upd {
                    ModuleUpd::Interact(ModuleInteract { payload, kind, .. }) => {
                        let Some(PowerProfiles) = payload.tag.downcast_ref() else {
                            continue;
                        };

                        match kind {
                            tui::InteractKind::Click(tui::MouseButton::Left) => {
                                cycle_tx.emit(CycleProfile);
                            }
                            _ => {
                                // TODO
                            }
                        }
                    }
                }
            }
        });

        if let Some(res) = tasks.join_next().await {
            res.context("Ppd module failed").ok_or_log();
        }
    }
}

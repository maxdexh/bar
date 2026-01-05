use std::sync::Arc;

use anyhow::Result;
use futures::Stream;
use tokio::sync::broadcast;
use tokio_stream::StreamExt as _;

use crate::utils::{ReloadRx, SharedEmit, lossy_broadcast};

pub fn connect(reload_rx: ReloadRx) -> (broadcast::Sender<()>, impl Stream<Item = Arc<str>>) {
    let (switch_tx, switch_rx) = broadcast::channel(50);
    let (profile_tx, profile_rx) = broadcast::channel(50);

    tokio::spawn(async move {
        match run(profile_tx, lossy_broadcast(switch_rx), reload_rx).await {
            Err(err) => log::error!("Failed to connect to ppd: {err}"),
            Ok(()) => log::warn!("Ppd client exited"),
        }
    });

    (switch_tx, lossy_broadcast(profile_rx))
}

async fn run(
    mut profile_tx: impl SharedEmit<Arc<str>>,
    switch_rx: impl Stream<Item = ()>,
    reload_rx: ReloadRx,
) -> Result<()> {
    let connection = zbus::Connection::system().await?;
    let proxy = dbus::PpdProxy::new(&connection).await?;

    enum Upd {
        ProfileChanged(Arc<str>),
        CycleProfile(()),
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

                if profile_tx.emit(cur_profile.clone()).is_break() {
                    break;
                }
            }
            Upd::Reload(()) => {
                if profile_tx.emit(cur_profile.clone()).is_break() {
                    break;
                }
            }
            Upd::CycleProfile(()) => {
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

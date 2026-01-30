use std::sync::Arc;

use anyhow::Context;
use futures::StreamExt as _;
use tokio::sync::Semaphore;
use tokio_util::task::AbortOnDropHandle;

use bar_common::utils::{ReloadRx, ResultExt, WatchRx, WatchTx, watch_chan};

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

pub struct PpdClient {
    pub profile_rx: WatchRx<Option<Arc<str>>>,
    cycle: Arc<Semaphore>,
    _background: AbortOnDropHandle<()>,
}
impl PpdClient {
    pub fn cycle_profile(&self) {
        self.cycle.add_permits(1);
    }
}

async fn run_bg(
    cycle_rx: Arc<Semaphore>,
    profile_tx: WatchTx<Option<Arc<str>>>,
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
            profile_tx.send_replace(profile.map(Into::into));
        }
    };

    let cycle_fut = async {
        loop {
            let Some(perm) = cycle_rx.acquire().await.ok_or_log() else {
                break;
            };
            perm.forget();
            let mut steps = 1;
            while let Some(perm) = cycle_rx.try_acquire().ok_or_debug() {
                perm.forget();
                steps += 1;
            }

            let Some((profiles, cur)) = tokio::try_join!(proxy.profiles(), proxy.active_profile())
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

pub fn connect(reload_rx: ReloadRx) -> PpdClient {
    let cycle = Arc::new(Semaphore::new(0));
    let (profile_tx, profile_rx) = watch_chan(Default::default());
    PpdClient {
        _background: AbortOnDropHandle::new(tokio::spawn(run_bg(
            cycle.clone(),
            profile_tx,
            reload_rx,
        ))),
        cycle,
        profile_rx,
    }
}

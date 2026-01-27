use std::time::Duration;

use anyhow::Context as _;
use futures::StreamExt as _;
use tokio_util::task::AbortOnDropHandle;
use zbus::proxy;

use crate::utils::{ReloadRx, ResultExt, WatchRx, WatchTx, run_or_retry, watch_chan};

macro_rules! declare_properties {
    (
        [$((
            $field_name:ident,
            $prop_name:expr,
            $typ:ty,
            $default:expr,
            $try_from_value:expr,
        )),* $(,)? ]
    ) => {
        #[derive(Clone, Debug)]
        pub struct UpowerState {
            $(pub $field_name: $typ,)*
            _p: (),
        }

        impl UpowerState {
            fn update(&mut self, prop_name: &str, value: zbus::zvariant::OwnedValue) -> anyhow::Result<bool> {
                match prop_name {
                    $($prop_name => {
                        let value = ($try_from_value)(value)?;
                        if self.$field_name == value {
                            return Ok(false);
                        }
                        self.$field_name = value;
                        Ok(true)
                    })*
                    _ => Ok(false),
                }
            }
        }

        impl Default for UpowerState {
            fn default() -> Self {
                Self {
                    $($field_name: ($default)(),)*
                    _p: (),
                }
            }
        }
    };
}
declare_properties!([
    (
        battery_state,
        "State",
        BatteryState,
        Default::default,
        TryFrom::try_from,
    ),
    (
        is_present,
        "IsPresent",
        bool,
        Default::default,
        TryFrom::try_from,
    ),
    (
        percentage,
        "Percentage",
        f64,
        Default::default,
        TryFrom::try_from,
    ),
    (
        energy_rate,
        "EnergyRate",
        f64,
        Default::default,
        TryFrom::try_from,
    ),
    (
        time_to_empty,
        "TimeToEmpty",
        Duration,
        Default::default,
        duration_from_value,
    ),
    (
        time_to_full,
        "TimeToFull",
        Duration,
        Default::default,
        duration_from_value,
    ),
]);

fn duration_from_value(value: zbus::zvariant::OwnedValue) -> anyhow::Result<Duration> {
    Ok(Duration::from_secs(
        i64::try_from(value)?
            .try_into()
            .context("Failed to convert negative duration")?,
    ))
}

#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, zbus::zvariant::OwnedValue)]
pub enum BatteryState {
    #[default]
    Unknown,
    Charging,
    Discharging,
    Empty,
    FullyCharged,
    PendingCharge,
    PendingDischarge,
}
impl TryFrom<&zbus::zvariant::Value<'_>> for BatteryState {
    type Error = zbus::zvariant::Error;

    fn try_from(value: &zbus::zvariant::Value<'_>) -> Result<Self, Self::Error> {
        let value = value.downcast_ref::<u32>()?;
        macro_rules! mk {
                ($(($variant:ident, $value:literal)),*) => {
                    match value {
                        $($value => Self::$variant,)*
                        _ => Self::Unknown,
                    }
                };
            }
        Ok(mk![
            (Charging, 1),
            (Discharging, 2),
            (Empty, 3),
            (FullyCharged, 4),
            (PendingCharge, 5),
            (PendingDischarge, 6)
        ])
    }
}

#[proxy(
    interface = "org.freedesktop.UPower.Device",
    default_service = "org.freedesktop.UPower",
    assume_defaults = false,
    gen_blocking = false
)]
trait Device {}

#[proxy(
    interface = "org.freedesktop.UPower",
    assume_defaults = true,
    gen_blocking = false
)]
trait UPower {
    #[zbus(object = "Device")]
    fn get_display_device(&self);
}

pub struct EnergyClient {
    pub state_rx: WatchRx<UpowerState>,
    _background: AbortOnDropHandle<()>,
}

async fn run_bg(state_tx: WatchTx<UpowerState>, mut reload_rx: ReloadRx) {
    run_or_retry(
        async |(state_tx, reload_rx)| try_run_bg(state_tx, reload_rx).await,
        (state_tx, reload_rx.clone()),
        |it| it.context("Failed to run upower client"),
        Duration::from_secs(60),
        Some(&mut reload_rx),
    )
    .await
}

async fn try_run_bg(
    state_tx: &WatchTx<UpowerState>,
    reload_rx: &mut ReloadRx,
) -> anyhow::Result<()> {
    let dbus = zbus::Connection::system().await?;

    let upower = UPowerProxy::<'static>::new(&dbus).await?;

    let device_proxy = upower.get_display_device().await?.into_inner();

    let properties = zbus::fdo::PropertiesProxy::builder(&dbus)
        .destination(device_proxy.destination())?
        .path(device_proxy.path())?
        .build()
        .await?;

    let device_interface = device_proxy.interface().to_owned();

    let _reload_task = AbortOnDropHandle::new({
        let state_tx = state_tx.clone();
        let properties = properties.clone();
        let mut reload_rx = reload_rx.clone();
        let device_interface = device_interface.clone();

        tokio::spawn(async move {
            while let Some(()) = reload_rx.wait().await {
                let Some(props) = properties
                    .get_all(device_interface.clone())
                    .await
                    .ok_or_log()
                else {
                    continue;
                };
                state_tx.send_modify(|state| {
                    for (member, value) in props {
                        state
                            .update(&member, value)
                            .context("Failed to update upower energy state")
                            .ok_or_log();
                    }
                });
            }
        })
    });

    let mut prop_change_rx = device_proxy.receive_all_signals().await?;
    while let Some(msg) = prop_change_rx.next().await {
        let state_tx = state_tx.clone();
        let device_interface = device_interface.clone();
        let properties = properties.clone();
        tokio::spawn(async move {
            let header = msg.header();
            let Some(member) = header.member() else {
                return;
            };

            let Some(value) = properties.get(device_interface, member).await.ok_or_log() else {
                return;
            };

            state_tx
                .send_if_modified(|state| state.update(member, value).ok_or_log().unwrap_or(false));
        });
    }

    Ok(())
}

pub fn connect(reload_rx: ReloadRx) -> EnergyClient {
    let (state_tx, state_rx) = watch_chan(Default::default());
    EnergyClient {
        _background: AbortOnDropHandle::new(tokio::spawn(run_bg(state_tx, reload_rx))),
        state_rx,
    }
}

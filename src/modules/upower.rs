use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use futures::Stream;
use serde::{Deserialize, Serialize};
use tokio_stream::StreamExt;
use tokio_util::task::AbortOnDropHandle;
pub use udbus::*;

use crate::tui;
use crate::utils::{
    Emit, ReloadRx, ReloadTx, ResultExt, WatchRx, WatchTx, stream_from_fn, watch_chan,
};

// https://upower.freedesktop.org/docs/UPower.html
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct EnergyState {
    pub bstate: BatteryState,
    pub rate: f64,
    pub percentage: f64,
    // corresponds to IsPresent for display device, see docs
    pub should_show: bool,
}
impl Default for EnergyState {
    fn default() -> Self {
        Self {
            bstate: BatteryState::Unknown,
            rate: 0.0,
            percentage: 100.0,
            should_show: false,
        }
    }
}

mod udbus {
    /// Originally taken from `upower-dbus` crate
    /// <https://github.com/pop-os/upower-dbus/blob/main/LICENSE>
    ///  Copyright 2021 System76 <info@system76.com>
    ///  SPDX-License-Identifier: MPL-2.0
    use serde::{Deserialize, Serialize};
    use zbus::proxy;
    use zbus::zvariant::{OwnedValue, Value};

    #[derive(Debug, Default, Copy, Clone, PartialEq, Eq, OwnedValue, Serialize, Deserialize)]
    #[repr(u32)]
    pub enum BatteryState {
        #[default]
        Unknown = 0,
        Charging = 1,
        Discharging = 2,
        Empty = 3,
        FullyCharged = 4,
        PendingCharge = 5,
        PendingDischarge = 6,
    }

    impl From<u32> for BatteryState {
        fn from(number: u32) -> Self {
            match number {
                n if n == BatteryState::Charging as u32 => BatteryState::Charging,
                n if n == BatteryState::Discharging as u32 => BatteryState::Discharging,
                n if n == BatteryState::Empty as u32 => BatteryState::Empty,
                n if n == BatteryState::FullyCharged as u32 => BatteryState::FullyCharged,
                n if n == BatteryState::PendingCharge as u32 => BatteryState::PendingCharge,
                n if n == BatteryState::PendingDischarge as u32 => BatteryState::PendingCharge,
                _ => BatteryState::Unknown,
            }
        }
    }

    impl TryFrom<&zbus::zvariant::Value<'_>> for BatteryState {
        type Error = zbus::zvariant::Error;

        fn try_from(value: &Value<'_>) -> Result<Self, Self::Error> {
            let value = value.downcast_ref::<u32>()?;
            Ok(value.into())
        }
    }

    #[derive(Debug, Copy, Clone, OwnedValue)]
    #[repr(u32)]
    pub enum BatteryType {
        Unknown = 0,
        LinePower = 1,
        Battery = 2,
        Ups = 3,
        Monitor = 4,
        Mouse = 5,
        Keyboard = 6,
        Pda = 7,
        Phone = 8,
    }

    #[derive(Debug, Copy, Clone, OwnedValue)]
    #[repr(u32)]
    pub enum BatteryLevel {
        Unknown = 0,
        None = 1,
        Low = 3,
        Critical = 4,
        Normal = 6,
        High = 7,
        Full = 8,
    }

    #[proxy(
        interface = "org.freedesktop.UPower.Device",
        default_service = "org.freedesktop.UPower",
        assume_defaults = false,
        gen_blocking = false
    )]
    pub trait Device {
        #[zbus(property)]
        fn battery_level(&self) -> zbus::Result<BatteryLevel>;

        #[zbus(property)]
        fn capacity(&self) -> zbus::Result<f64>;

        #[zbus(property)]
        fn energy(&self) -> zbus::Result<f64>;

        #[zbus(property)]
        fn energy_empty(&self) -> zbus::Result<f64>;

        #[zbus(property)]
        fn energy_full(&self) -> zbus::Result<f64>;

        #[zbus(property)]
        fn energy_full_design(&self) -> zbus::Result<f64>;

        #[zbus(property)]
        fn energy_rate(&self) -> zbus::Result<f64>;

        #[zbus(property)]
        fn has_history(&self) -> zbus::Result<bool>;

        #[zbus(property)]
        fn has_statistics(&self) -> zbus::Result<bool>;

        #[zbus(property)]
        fn icon_name(&self) -> zbus::Result<String>;

        #[zbus(property)]
        fn is_present(&self) -> zbus::Result<bool>;

        #[zbus(property)]
        fn is_rechargeable(&self) -> zbus::Result<bool>;

        #[zbus(property)]
        fn luminosity(&self) -> zbus::Result<f64>;

        #[zbus(property)]
        fn model(&self) -> zbus::Result<String>;

        #[zbus(property)]
        fn native_path(&self) -> zbus::Result<String>;

        #[zbus(property)]
        fn online(&self) -> zbus::Result<bool>;

        #[zbus(property)]
        fn percentage(&self) -> zbus::Result<f64>;

        #[zbus(property)]
        fn power_supply(&self) -> zbus::Result<bool>;

        fn refresh(&self) -> zbus::Result<()>;

        #[zbus(property)]
        fn serial(&self) -> zbus::Result<String>;

        #[zbus(property)]
        fn state(&self) -> zbus::Result<BatteryState>;

        #[zbus(property)]
        fn temperature(&self) -> zbus::Result<f64>;

        #[zbus(property, name = "Type")]
        fn type_(&self) -> zbus::Result<BatteryType>;

        #[zbus(property)]
        fn vendor(&self) -> zbus::Result<String>;

        #[zbus(property)]
        fn voltage(&self) -> zbus::Result<f64>;
    }

    #[proxy(
        interface = "org.freedesktop.UPower",
        assume_defaults = true,
        gen_blocking = false
    )]
    pub trait UPower {
        /// EnumerateDevices method
        fn enumerate_devices(&self) -> zbus::Result<Vec<zbus::zvariant::OwnedObjectPath>>;

        /// GetCriticalAction method
        fn get_critical_action(&self) -> zbus::Result<String>;

        /// GetDisplayDevice method
        #[zbus(object = "Device")]
        fn get_display_device(&self);

        #[zbus(name = "GetDisplayDevice")]
        fn get_display_device_(&self) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;

        /// DeviceAdded signal
        #[zbus(signal)]
        fn device_added(&self, device: zbus::zvariant::ObjectPath<'_>) -> zbus::Result<()>;

        /// DeviceRemoved signal
        #[zbus(signal)]
        fn device_removed(&self, device: zbus::zvariant::ObjectPath<'_>) -> zbus::Result<()>;

        /// DaemonVersion property
        #[zbus(property)]
        fn daemon_version(&self) -> zbus::Result<String>;

        /// LidIsClosed property
        #[zbus(property)]
        fn lid_is_closed(&self) -> zbus::Result<bool>;

        /// LidIsPresent property
        #[zbus(property)]
        fn lid_is_present(&self) -> zbus::Result<bool>;

        /// OnBattery property
        #[zbus(property)]
        fn on_battery(&self) -> zbus::Result<bool>;
    }
}

use crate::modules::prelude::*;
pub struct EnergyModule {
    state_rx: WatchRx<EnergyState>,
    reload_tx: ReloadTx,
    _background: AbortOnDropHandle<()>,
}

impl EnergyModule {
    async fn run_bg(state_tx: WatchTx<EnergyState>, mut reload_rx: ReloadRx) {
        let device = loop {
            let Some(it) = async {
                let dbus = zbus::Connection::system().await?;
                let upower = UPowerProxy::<'static>::new(&dbus).await?;
                let device = upower.get_display_device().await?;
                anyhow::Ok(device)
            }
            .await
            .ok_or_log() else {
                tokio::time::sleep(Duration::from_secs(60)).await;
                continue;
            };

            break it;
        };

        enum Upd {
            Reload(()),
            Rate(f64),
            Percentage(f64),
            ShouldShow(bool),
            BState(BatteryState),
        }

        fn stream_prop<'a, T: TryFrom<zbus::zvariant::OwnedValue, Error: Into<zbus::Error>>>(
            prop: impl Stream<Item = zbus::proxy::PropertyChanged<'a, T>>,
            tf: impl Fn(T) -> Upd,
            propname: &str,
        ) -> impl Stream<Item = Upd> {
            prop.then(|opt| async move { opt.get().await })
                .filter_map(move |it| {
                    it.with_context(|| format!("Failed to get {propname}"))
                        .ok_or_log()
                })
                .map(tf)
        }

        // TODO: Use Proxy::receive_property_changed?
        let updates = stream_from_fn(async move || reload_rx.wait().await)
            .map(Upd::Reload)
            .merge(stream_prop(
                device.receive_energy_rate_changed().await,
                Upd::Rate,
                "wattage",
            ))
            .merge(stream_prop(
                device.receive_percentage_changed().await,
                Upd::Percentage,
                "percentage",
            ))
            .merge(stream_prop(
                device.receive_is_present_changed().await,
                Upd::ShouldShow,
                "isPresent attribute",
            ))
            .merge(stream_prop(
                device.receive_state_changed().await,
                Upd::BState,
                "charge state",
            ));
        tokio::pin!(updates);

        while let Some(update) = updates.next().await {
            state_tx.send_modify(|state| match update {
                Upd::Reload(()) => (),
                Upd::Rate(rate) => state.rate = rate,
                Upd::Percentage(percentage) => state.percentage = percentage,
                Upd::ShouldShow(should_show) => state.should_show = should_show,
                Upd::BState(bstate) => state.bstate = bstate,
            })
        }
    }
}

impl Module for EnergyModule {
    type Config = ();

    fn connect() -> Self {
        let (state_tx, state_rx) = watch_chan(Default::default());
        let reload_tx = ReloadTx::new();
        Self {
            _background: AbortOnDropHandle::new(tokio::spawn(Self::run_bg(
                state_tx,
                reload_tx.subscribe(),
            ))),
            state_rx,
            reload_tx,
        }
    }

    async fn run_module_instance(
        self: Arc<Self>,
        cfg: Self::Config,
        ModuleArgs {
            mut act_tx,
            mut reload_rx,
            ..
        }: ModuleArgs,
        _cancel: crate::utils::CancelDropGuard,
    ) {
        let mut state_rx = self.state_rx.clone();
        let mut reload_tx = self.reload_tx.clone();

        let render_fut = async {
            while let Ok(()) = state_rx.changed().await {
                let energy = state_rx.borrow_and_update();
                if !energy.should_show {
                    act_tx.emit(ModuleAct::HideModule);
                    continue;
                }

                // TODO: Time estimate tooltip
                let percentage = energy.percentage.round() as i64;
                let sign = match energy.bstate {
                    BatteryState::Discharging | BatteryState::PendingDischarge => '-',
                    _ => '+',
                };
                let rate = format!("{sign}{:.1}W", energy.rate);
                let energy = format!("{percentage:>3}% {rate:>6}");

                act_tx.emit(ModuleAct::RenderAll(tui::StackItem::auto(
                    tui::Text::plain(energy),
                )));
            }
        };

        tokio::select! {
            () = render_fut => (),
            () = reload_tx.reload_on(&mut reload_rx) => (),
        }
    }
}

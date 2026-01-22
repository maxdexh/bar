use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use tokio_stream::StreamExt;
use tokio_util::task::AbortOnDropHandle;
pub use udbus::*;

use crate::tui;
use crate::utils::{Emit, ReloadRx, ReloadTx, ResultExt, WatchRx, WatchTx, unb_chan, watch_chan};

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
            $($field_name: $typ,)*
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
        battery_level,
        "BatteryLevel",
        BatteryLevel,
        Default::default,
        TryFrom::try_from,
    ),
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

    #[derive(Debug, Default, Copy, Clone, PartialEq, Eq, OwnedValue, Serialize, Deserialize)]
    #[repr(u32)]
    pub enum BatteryType {
        #[default]
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

    #[derive(Debug, Default, Copy, Clone, PartialEq, Eq, OwnedValue, Serialize, Deserialize)]
    #[repr(u32)]
    pub enum BatteryLevel {
        #[default]
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
    state_rx: WatchRx<UpowerState>,
    reload_tx: ReloadTx,
    _background: AbortOnDropHandle<()>,
}

impl EnergyModule {
    async fn run_bg(state_tx: WatchTx<UpowerState>, mut reload_rx: ReloadRx) {
        let mut run_fallible = async || {
            let dbus = zbus::Connection::system().await.ok_or_log()?;
            let upower = UPowerProxy::<'static>::new(&dbus).await.ok_or_log()?;

            let device = upower.get_display_device().await.ok_or_log()?;
            let device_proxy = device.inner();
            let properties = zbus::fdo::PropertiesProxy::builder(&dbus)
                .destination(device_proxy.destination())
                .ok_or_log()?
                .path(device_proxy.path())
                .ok_or_log()?
                .build()
                .await
                .ok_or_log()?;

            let prop_change_rx = device_proxy.receive_all_signals().await.ok_or_log()?;
            let main_fut =
                futures::StreamExt::for_each_concurrent(prop_change_rx, 10, async |msg| {
                    let header = msg.header();
                    let Some(member) = header.member() else {
                        return;
                    };

                    let Some(value) = properties
                        .get(device_proxy.interface().clone(), member)
                        .await
                        .ok_or_log()
                    else {
                        return;
                    };

                    state_tx.send_if_modified(|state| {
                        state.update(member, value).ok_or_log().unwrap_or(false)
                    });
                });
            let reload_fut = async {
                while let Some(()) = reload_rx.wait().await {
                    let Some(props) = properties
                        .get_all(device_proxy.interface().clone())
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
            };

            tokio::select! {
                () = main_fut => (),
                () = reload_fut => (),
            }

            Some(())
        };

        loop {
            if let Some(()) = run_fallible().await {
                break;
            };

            tokio::time::sleep(Duration::from_secs(60)).await;
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
            act_tx,
            mut reload_rx,
            ..
        }: ModuleArgs,
        _cancel: crate::utils::CancelDropGuard,
    ) {
        let state_rx = self.state_rx.clone();
        let mut reload_tx = self.reload_tx.clone();

        let (interact_tx, mut interact_rx) = unb_chan();

        let on_interact =
            tui::InteractCallback::from_fn(move |interact| interact_tx.emit(interact));

        let render_fut = async {
            let mut state_rx = state_rx.clone();
            while let Ok(()) = state_rx.changed().await {
                let energy = state_rx.borrow_and_update();
                if !energy.is_present {
                    act_tx.emit(ModuleAct::HideModule);
                    continue;
                }

                // TODO: Time estimate tooltip
                let percentage = energy.percentage.round() as i64;
                let sign = match energy.battery_state {
                    BatteryState::Discharging | BatteryState::PendingDischarge => '-',
                    BatteryState::Charging | BatteryState::PendingCharge => '+',
                    BatteryState::FullyCharged | BatteryState::Unknown | BatteryState::Empty => '±',
                };
                let rate = format!("{sign}{:.1}W", energy.energy_rate);
                let energy = format!("{percentage:>3}% {rate:<6}");

                //payload: tui::InteractPayload {
                //    mod_inst: inst_id.clone(),
                //    tag: tui::InteractTag::new(EnergyInteractTag),
                //},
                act_tx.emit(ModuleAct::RenderAll(tui::StackItem::auto(
                    tui::Elem::from(tui::RawPrint::plain(energy)).on_interact(on_interact.clone()),
                )));
            }
        };
        let menu_fut = async {
            while let Some(tui::InteractData {
                location,
                monitor,
                kind,
                ..
            }) = interact_rx.next().await
            {
                let text = {
                    let lock = state_rx.borrow();
                    let display_time = |time: Duration| {
                        let hours = time.as_secs() / 3600;
                        let mins = (time.as_secs() / 60) % 60;
                        format!("{hours}h {mins}min")
                    };
                    match lock.battery_state {
                        BatteryState::Discharging | BatteryState::PendingDischarge => {
                            format!("Battery empty in {}", display_time(lock.time_to_empty))
                        }
                        BatteryState::FullyCharged => "Battery full".to_owned(),
                        BatteryState::Empty => "Battery empty".to_owned(),
                        BatteryState::Unknown => "Battery state unknown".to_owned(),
                        BatteryState::Charging | BatteryState::PendingCharge => {
                            format!("Battery full in {}", display_time(lock.time_to_full))
                        }
                    }
                };

                if let tui::InteractKind::Hover = kind {
                    act_tx.emit(ModuleAct::OpenMenu(OpenMenu {
                        monitor,
                        tui: tui::RawPrint::plain(text).into(),
                        location,
                        menu_kind: MenuKind::Tooltip,
                    }))
                }
            }
        };

        tokio::select! {
            () = render_fut => {}
            () = menu_fut => {}
            () = reload_tx.reload_on(&mut reload_rx) => {}
        }
    }
}

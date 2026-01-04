use std::{collections::HashMap, sync::Arc};

use crate::utils::SharedEmit;

#[derive(PartialEq, Clone, Debug)]
pub struct MonitorInfo {
    pub name: Arc<str>,
    pub scale: f64,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Default)]
pub struct MonitorEvent {
    data: Arc<HashMap<Arc<str>, MonitorInfo>>,
    prev: Arc<HashMap<Arc<str>, MonitorInfo>>,
}
impl MonitorEvent {
    pub fn removed(&self) -> impl Iterator<Item = &str> {
        self.prev
            .keys()
            .filter(|&it| !self.data.contains_key(it))
            .map(|name| &**name)
    }
    pub fn added(&self) -> impl Iterator<Item = &MonitorInfo> {
        self.data
            .values()
            .filter(|&it| !self.prev.contains_key(&it.name))
    }
    pub fn changed(&self) -> impl Iterator<Item = &MonitorInfo> {
        self.data
            .values()
            .filter(|&it| !self.prev.get(&it.name).is_some_and(|v| v != it))
    }
}

// TODO: Use wl-client instead
pub fn connect(mut tx: impl SharedEmit<MonitorEvent>) {
    std::thread::spawn(move || {
        let sleep_ms = |ms| std::thread::sleep(std::time::Duration::from_millis(ms));
        let sleep_err = || sleep_ms(2000);

        #[derive(serde::Deserialize)]
        struct MonitorData {
            name: Arc<str>,
            scale: f64,
            modes: Vec<MonitorMode>,
            enabled: bool,
        }
        #[derive(serde::Deserialize)]
        struct MonitorMode {
            width: u32,
            height: u32,
            current: bool,
        }

        let mut cur_displays = Default::default();
        loop {
            let Ok(std::process::Output {
                status,
                stdout,
                stderr,
            }) = std::process::Command::new("wlr-randr")
                .arg("--json")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .output()
                .map_err(|err| log::error!("Failed to run wlr-randr: {err}"))
            else {
                sleep_err();
                continue;
            };

            if !status.success() {
                log::error!(
                    "wlr-randr --json exited with exit code {status}. Stderr: {}",
                    String::from_utf8_lossy(&stderr),
                );
                sleep_err();
                continue;
            }

            let Ok(data) = serde_json::from_slice::<Vec<MonitorData>>(&stdout).map_err(|err| {
                log::error!("Failed to deserialize output of wlr-randr --json: {err}")
            }) else {
                sleep_err();
                continue;
            };

            let displays = Arc::new(
                data.into_iter()
                    .filter(|md| md.enabled)
                    .filter_map(|md| {
                        let MonitorData {
                            name, scale, modes, ..
                        } = md;
                        let MonitorMode { width, height, .. } =
                            modes.into_iter().find(|it| it.current)?;
                        Some((
                            name.clone(),
                            MonitorInfo {
                                name,
                                scale,
                                width,
                                height,
                            },
                        ))
                    })
                    .collect(),
            );
            if displays != cur_displays {
                if tx
                    .emit(MonitorEvent {
                        prev: Arc::clone(&cur_displays),
                        data: displays.clone(),
                    })
                    .is_break()
                {
                    break;
                }

                cur_displays = displays;
            }

            sleep_ms(5000);
        }
    });
}

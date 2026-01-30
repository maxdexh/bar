use std::{collections::HashMap, sync::Arc};

use futures::Stream;

use bar_common::utils::unb_chan;

#[derive(PartialEq, Clone, Debug)]
pub struct MonitorInfo {
    pub name: Arc<str>,
    pub scale: f64,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Default, Clone)]
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
    pub fn added_or_changed(&self) -> impl Iterator<Item = &MonitorInfo> {
        self.data
            .values()
            .filter(|&it| self.prev.get(&it.name).is_none_or(|v| v != it))
    }
}

#[derive(Default)]
struct State {
    data: Arc<HashMap<Arc<str>, MonitorInfo>>,
}
impl State {
    fn refresh(&mut self) -> anyhow::Result<Option<MonitorEvent>> {
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

        let std::process::Output {
            status,
            stdout,
            stderr,
        } = std::process::Command::new("wlr-randr")
            .arg("--json")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .map_err(|err| anyhow::anyhow!("Failed to run wlr-randr --json: {err}"))?;

        if !status.success() {
            anyhow::bail!(
                "wlr-randr --json exited with exit code {status}. Stderr: {}",
                String::from_utf8_lossy(&stderr),
            );
        }

        let data = serde_json::from_slice::<Vec<MonitorData>>(&stdout).map_err(|err| {
            anyhow::anyhow!("Failed to deserialize output of wlr-randr --json: {err}")
        })?;

        let data = Arc::new(
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
        Ok(if data != self.data {
            let old_data = std::mem::replace(&mut self.data, data.clone());
            Some(MonitorEvent {
                data,
                prev: old_data,
            })
        } else {
            None
        })
    }
}

// FIXME: Use a watch channel instead
pub fn connect() -> impl Stream<Item = MonitorEvent> {
    let (tx, rx) = unb_chan();
    std::thread::spawn(move || {
        let mut state = State::default();
        loop {
            match state.refresh() {
                Ok(ev) => {
                    if let Some(ev) = ev
                        && tx.send(ev).is_err()
                    {
                        break;
                    }
                }
                Err(err) => {
                    log::error!("{err}");
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    });
    rx
}

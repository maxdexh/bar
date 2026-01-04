use std::{collections::HashSet, sync::Arc};

use futures::Stream;
use tokio::sync::mpsc;

#[derive(Debug)]
pub struct DisplayDiff {
    pub added: HashSet<Arc<str>>,
    pub removed: HashSet<Arc<str>>,
}

// TODO: Use wl-client instead
pub fn connect() -> impl Stream<Item = DisplayDiff> {
    let (tx, mut rx) = mpsc::unbounded_channel();

    std::thread::spawn(move || {
        let sleep_ms = |ms| std::thread::sleep(std::time::Duration::from_millis(ms));
        let sleep_err = || sleep_ms(2000);

        #[derive(serde::Deserialize)]
        struct DisplayData {
            name: Arc<str>,
        }

        let mut cur_displays = HashSet::new();
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
                    String::from_utf8_lossy(&stderr)
                );
                sleep_err();
                continue;
            }

            let Ok(data) = serde_json::from_slice::<Vec<DisplayData>>(&stdout).map_err(|err| {
                log::error!("Failed to deserialize output of wlr-randr --json: {err}")
            }) else {
                sleep_err();
                continue;
            };

            let displays: HashSet<_> = data.into_iter().map(|DisplayData { name }| name).collect();
            if displays != cur_displays {
                let diff = DisplayDiff {
                    added: displays.difference(&cur_displays).cloned().collect(),
                    removed: {
                        cur_displays.retain(|it| !displays.contains(it));
                        cur_displays
                    },
                };
                cur_displays = displays;

                log::debug!("Sending display diff: {diff:?}");
                if let Err(err) = tx.send(diff) {
                    log::warn!("Display output listener exited: {err}");
                    break;
                }
            }

            sleep_ms(5000);
        }
    });

    futures::stream::poll_fn(move |cx| rx.poll_recv(cx))
}

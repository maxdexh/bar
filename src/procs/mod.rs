use std::{borrow::Borrow as _, ffi::OsString, path::Path, sync::Arc};

use anyhow::{Result, anyhow};
use futures::Stream;
use serde::{Serialize, de::DeserializeOwned};
use tempfile::TempDir;
use tokio::{
    io::{AsyncBufReadExt as _, AsyncWriteExt as _},
    sync::mpsc,
    task::JoinSet,
};
use tokio_stream::StreamExt as _;

use crate::logging::{ProcKindForLogger, init_logger};

pub mod bar_panel;
pub mod controller;
pub mod menu_panel;

const INTERNAL_BAR_PANEL_ARG: &str = "bar";
const INTERNAL_MENU_ARG: &str = "menu";
const EDGE: &str = "top";

const BASE_DIR_ENV: &str = "BAR_BASE_DIR";
const SOCKET_PATH_ENV: &str = "BAR_PANEL_SOCKET_PATH";
const MONITOR_NAME_ENV: &str = "BAR_MONITOR_NAME";

// FIXME: Simplify after display panel migration
async fn run_panel_controller_side<Ev, Upd>(
    socket_name: &str,
    display: Arc<str>,
    ev_tx: mpsc::UnboundedSender<Ev>,
    updates: impl Stream<Item: std::borrow::Borrow<Upd> + Send> + Send + 'static,
    spawn_panel: impl AsyncFnOnce(
        &Path,
        &str,
        Vec<(OsString, OsString)>,
        &mpsc::UnboundedSender<Ev>,
    ) -> anyhow::Result<tokio::process::Child>,
) -> anyhow::Result<()>
where
    Upd: Send + Sync + Serialize + 'static,
    Ev: Send + DeserializeOwned + 'static,
{
    let dir_guard = TempDir::new()?;
    let dir = dir_guard.path();
    log::debug!("Started panel on {display} with files at {}", dir.display());
    let socket_path = dir.join(socket_name);
    let listener = tokio::net::UnixListener::bind(&socket_path)?;
    let mut child = spawn_panel(
        dir,
        &display,
        vec![
            (MONITOR_NAME_ENV.into(), display.as_ref().into()),
            (BASE_DIR_ENV.into(), dir.into()),
            (SOCKET_PATH_ENV.into(), socket_path.into()),
        ],
        &ev_tx,
    )
    .await?;

    let (stream, _) = listener.accept().await?;
    let (ev_read, mut upd_write) = stream.into_split();

    let mut tasks = tokio::task::JoinSet::<()>::new();
    tasks.spawn(async move {
        tokio::pin!(updates);
        while let Some(update) = updates.next().await {
            let Ok(buf) = postcard::to_stdvec_cobs(update.borrow())
                .map_err(|err| log::error!("Failed to serialize update: {err}"))
            else {
                continue;
            };

            if let Err(err) = upd_write.write_all(&buf).await {
                log::error!("Failed to write to update socket: {err}");
                break;
            }
        }
    });
    tasks.spawn(read_cobs_sock(ev_read, ev_tx));

    tasks.join_next().await;
    tasks.abort_all();

    match tokio::time::timeout(tokio::time::Duration::from_secs(5), child.wait()).await {
        Err(err) => {
            log::error!(
                "Killing panel {socket_name} on {display} that failed to exit in time: {err}"
            );
            child.kill().await?;
        }
        Ok(res) => {
            let status = res?;
            if !status.success() {
                log::warn!("Panel {socket_name} on {display} exited with code {status}")
            }
        }
    }

    log::info!("Panel on {display} exited");
    Ok(())
}

async fn read_cobs_sock<T: DeserializeOwned>(
    read: tokio::net::unix::OwnedReadHalf,
    tx: mpsc::UnboundedSender<T>,
) {
    let mut read = tokio::io::BufReader::new(read);
    loop {
        let mut buf = Vec::new();
        match read.read_until(0, &mut buf).await {
            Ok(0) => break,
            Err(err) => {
                log::error!("Failed to read event socket: {err}");
                break;
            }
            Ok(n) => log::trace!("Received {n} bytes"),
        }

        match postcard::from_bytes_cobs(&mut buf) {
            Err(err) => {
                log::error!(
                    "Failed to deserialize {} from socket: {err}",
                    std::any::type_name::<T>()
                );
            }
            Ok(ev) => {
                if let Err(err) = tx.send(ev) {
                    log::info!("Closing reader: {err}");
                    break;
                }
            }
        }

        buf.clear();
    }
}

async fn write_cobs_sock<T: Serialize>(
    mut write: tokio::net::unix::OwnedWriteHalf,
    mut rx: mpsc::UnboundedReceiver<T>,
) {
    while let Some(item) = rx.recv().await {
        let Ok(buf) = postcard::to_stdvec_cobs(&item)
            .map_err(|err| log::error!("Failed to serialize update: {err}"))
        else {
            continue;
        };

        if let Err(err) = write.write_all(&buf).await {
            log::error!("Failed to write to update socket: {err}");
            break;
        }
    }
}

pub async fn entry_point() -> Result<()> {
    let mut args = std::env::args().skip(1);

    match args.next().as_deref() {
        Some("internal") => {
            let mode = args.next().ok_or_else(|| anyhow!("Missing mode arg"))?;

            let socket_path = std::env::var_os(SOCKET_PATH_ENV)
                .ok_or_else(|| anyhow!("Missing {SOCKET_PATH_ENV}"))?;
            let monitor = std::env::var(MONITOR_NAME_ENV)
                .map_err(|err| anyhow!("{MONITOR_NAME_ENV}: {err}"))?;

            let (upd_read, ev_write) = tokio::net::UnixStream::connect(socket_path)
                .await?
                .into_split();

            let mut tasks = JoinSet::<()>::new();

            // FIXME: Merge code after display panel migration
            let handle_main = match mode.as_str() {
                INTERNAL_BAR_PANEL_ARG => {
                    init_logger(ProcKindForLogger::Bar(monitor.clone()));
                    let (upd_tx, upd_rx) = mpsc::unbounded_channel();
                    let (ev_tx, ev_rx) = mpsc::unbounded_channel();
                    tasks.spawn(read_cobs_sock(upd_read, upd_tx));
                    tasks.spawn(write_cobs_sock(ev_write, ev_rx));

                    let (fut_main, handle_main) = futures::future::abortable(async move {
                        if let Err(err) =
                            crate::display_panel::main(ev_tx, upd_rx, monitor.into()).await
                        {
                            log::error!("Bar panel failed: {err}");
                        }
                    });
                    let fut_io = tasks.join_next();
                    tokio::pin!(fut_io, fut_main);
                    _ = futures::future::select(fut_main, fut_io).await;
                    handle_main
                }
                INTERNAL_MENU_ARG => {
                    init_logger(ProcKindForLogger::Menu(monitor));
                    let (upd_tx, upd_rx) = mpsc::unbounded_channel();
                    let (ev_tx, ev_rx) = mpsc::unbounded_channel();
                    tasks.spawn(read_cobs_sock(upd_read, upd_tx));
                    tasks.spawn(write_cobs_sock(ev_write, ev_rx));

                    let (fut_main, handle_main) = futures::future::abortable(async move {
                        if let Err(err) = menu_panel::main(ev_tx, upd_rx).await {
                            log::error!("Menu panel failed: {err}");
                        }
                    });
                    let fut_io = tasks.join_next();
                    tokio::pin!(fut_io, fut_main);
                    _ = futures::future::select(fut_main, fut_io).await;
                    handle_main
                }
                _ => return Err(anyhow!("Bad arguments")),
            };

            handle_main.abort();
            tasks.abort_all();

            Ok(())
        }
        None => {
            init_logger(ProcKindForLogger::Controller);

            controller::main().await
        }
        _ => Err(anyhow!("Bad arguments")),
    }
}

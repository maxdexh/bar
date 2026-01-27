use std::{
    ffi::{OsStr, OsString},
    path::Path,
    time::Duration,
};

use anyhow::Context as _;
use futures::{Stream, StreamExt as _};
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;
use tokio_util::{sync::CancellationToken, time::FutureExt as _};

use crate::{
    tui,
    utils::{CancelDropGuard, ResultExt as _, UnbTx, unb_chan},
};

const SOCK_PATH_VAR: &str = "BAR_TERM_INSTANCE_SOCK_PATH";
pub const PANEL_PROC_ARG: &str = "--internal-managed-terminal";
pub const PROC_LOG_NAME_VAR: &str = "BAR_TERM_INSTANCE_NAME";

#[derive(Serialize, Deserialize, Debug)]
pub enum TermUpdate {
    Print(Vec<u8>),
    Flush,
    RemoteControl(Vec<OsString>),
    Shell(OsString, Vec<OsString>), // TODO: Envs
}

#[derive(Serialize, Deserialize, Debug)]
pub enum TermEvent {
    Crossterm(crossterm::event::Event),
    Sizes(tui::Sizes),
}

pub async fn start_generic_panel(
    sock_path: &Path,
    log_name: &str,
    upd_rx: impl Stream<Item = TermUpdate> + 'static + Send,
    extra_args: impl IntoIterator<Item: AsRef<OsStr>>,
    extra_envs: impl IntoIterator<Item = (OsString, OsString)>,
    term_ev_tx: UnbTx<TermEvent>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let socket = tokio::net::UnixListener::bind(sock_path)?;

    let mut child = tokio::process::Command::new("kitten")
        .arg("panel")
        .args(extra_args)
        .arg(std::env::current_exe()?)
        .arg(PANEL_PROC_ARG)
        .envs(extra_envs)
        .env(SOCK_PATH_VAR, sock_path)
        .env(PROC_LOG_NAME_VAR, log_name)
        .kill_on_drop(true)
        .stdout(std::io::stderr())
        .spawn()
        .context("Failed to spawn terminal")?;

    let (socket, _) = socket
        .accept()
        .await
        .context("Failed to accept socket connection")?;

    tokio::spawn(async move {
        let mut mgr = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(run_term_inst_mgr(
            socket,
            term_ev_tx,
            upd_rx,
            cancel.clone(),
        )));
        tokio::select! {
            exit_res = child.wait() => {
                exit_res.context("Failed to wait for terminal exit").ok_or_log();
            }
            () = cancel.cancelled() => {}
            run_res = &mut mgr => {
                run_res
                    .context("Terminal instance failed")
                    .ok_or_log();
            }
        };
        cancel.cancel();
        drop(mgr);

        // Child should exit by itself because the socket connection is closed.
        let child_res = child.wait().timeout(Duration::from_secs(10)).await;

        match (|| anyhow::Ok(child_res??))()
            .context("Terminal instance failed to exit after shutdown")
            .ok_or_log()
        {
            Some(status) => {
                if !status.success() {
                    log::error!("Terminal exited with nonzero status {status}");
                }
            }
            None => {
                child
                    .kill()
                    .await
                    .context("Failed to kill terminal")
                    .ok_or_log();
            }
        }
    });

    Ok(())
}
async fn run_term_inst_mgr(
    connection: tokio::net::UnixStream,
    ev_tx: UnbTx<TermEvent>,
    updates: impl Stream<Item = TermUpdate> + Send + 'static,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let _auto_cancel = CancelDropGuard::from(cancel.clone());
    let mut tasks = JoinSet::<()>::new();
    // TODO: Await stream

    let (read_half, write_half) = connection.into_split();

    tasks.spawn(read_cobs_sock::<TermEvent>(
        read_half,
        move |x| {
            ev_tx.send(x).ok_or_debug();
        },
        cancel.clone(),
    ));
    tasks.spawn(write_cobs_sock::<TermUpdate>(
        write_half,
        updates,
        cancel.clone(),
    ));

    if let Some(Err(err)) = tasks.join_next().await {
        log::error!("Error with task: {err}");
    }
    cancel.cancel();
    tasks.join_all().await;

    Ok(())
}

pub async fn term_proc_main() -> std::process::ExitCode {
    match term_proc_main_inner().await.ok_or_log() {
        Some(()) => std::process::ExitCode::SUCCESS,
        None => std::process::ExitCode::FAILURE,
    }
}

async fn term_proc_main_inner() -> anyhow::Result<()> {
    let mut tasks = JoinSet::new();
    let cancel = CancellationToken::new();
    let (ev_tx, upd_rx);
    {
        let socket = std::env::var_os(SOCK_PATH_VAR).context("Missing socket path env var")?;
        let socket = tokio::net::UnixStream::connect(socket)
            .await
            .context("Failed to connect to socket")?;
        let (read, write) = socket.into_split();

        let (upd_tx, ev_rx);
        (ev_tx, ev_rx) = unb_chan::<TermEvent>();
        (upd_tx, upd_rx) = std::sync::mpsc::channel::<TermUpdate>();

        tasks.spawn(read_cobs_sock(
            read,
            move |x| {
                upd_tx.send(x).ok_or_debug();
            },
            cancel.clone(),
        ));
        tasks.spawn(write_cobs_sock(write, ev_rx, cancel.clone()));
    }

    crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::EnterAlternateScreen,
        crossterm::cursor::Hide,
        crossterm::event::EnableMouseCapture,
    )?;
    crossterm::terminal::enable_raw_mode()?;

    let Some(init_sizes) = tui::Sizes::query()? else {
        anyhow::bail!("Terminal reported window size of 0. Do not start as hidden!");
    };

    ev_tx
        .send(TermEvent::Sizes(init_sizes))
        .context("Failed to send initial font size while starting panel. Exiting.")?;

    tasks.spawn(async move {
        let events = crossterm::event::EventStream::new()
            .filter_map(async |res| res.context("Crossterm error").ok_or_log());
        tokio::pin!(events);
        while let Some(ev) = events.next().await {
            if let crossterm::event::Event::Resize(_, _) = &ev
                && let Some(sizes) = tui::Sizes::query().ok_or_log()
            {
                if let Some(sizes) = sizes {
                    ev_tx.send(TermEvent::Sizes(sizes)).ok_or_debug();
                } else {
                    log::debug!(
                        "Terminal reported window size of 0 (this is expected if the terminal is hidden)"
                    );
                }
            }
            ev_tx.send(TermEvent::Crossterm(ev)).ok_or_debug();
        }
    });

    fn run_cmd(cmd: &mut std::process::Command) {
        if let Err(err) = (|| {
            let std::process::Output {
                status,
                stdout: _,
                stderr,
            } = cmd.output()?;

            if !status.success() {
                anyhow::bail!(
                    "Exited with status {status}. Stderr:\n{}",
                    String::from_utf8_lossy(&stderr)
                );
            }
            Ok(())
        })() {
            log::error!("Failed to run command {cmd:?}: {err}")
        }
    }

    let cancel_blocking = cancel.clone();
    std::thread::spawn(move || {
        let auto_cancel = CancelDropGuard::from(cancel_blocking);
        use std::io::Write as _;
        let mut stdout = std::io::BufWriter::new(std::io::stdout().lock());
        while !auto_cancel.inner.is_cancelled()
            && let Ok(upd) = upd_rx.recv()
        {
            match upd {
                TermUpdate::Print(bytes) => {
                    stdout
                        .write_all(&bytes)
                        .context("Failed to print")
                        .ok_or_log();
                }
                TermUpdate::Flush => {
                    stdout.flush().context("Failed to flush").ok_or_log();
                }
                TermUpdate::RemoteControl(args) => {
                    let Some(listen_on) = std::env::var_os("KITTY_LISTEN_ON")
                        .context("Missing KITTY_LISTEN_ON")
                        .ok_or_log()
                    else {
                        continue;
                    };
                    run_cmd(
                        std::process::Command::new("kitten")
                            .arg("@")
                            .arg("--to")
                            .arg(listen_on)
                            .args(args),
                    );
                }
                TermUpdate::Shell(cmd, args) => {
                    run_cmd(std::process::Command::new(cmd).args(args));
                }
            }
        }
    });

    tokio::select! {
        Some(res) = tasks.join_next() => {
            res.ok_or_log();
        }
        () = cancel.cancelled() => {}
    }

    Ok(())
}

async fn read_cobs_sock<T: serde::de::DeserializeOwned>(
    read: tokio::net::unix::OwnedReadHalf,
    tx: impl Fn(T),
    cancel: CancellationToken,
) {
    let auto_cancel = CancelDropGuard::from(cancel);
    async {
        use tokio::io::AsyncBufReadExt as _;
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
                    tx(ev);
                }
            }

            buf.clear();
        }
    }
    .with_cancellation_token(&auto_cancel.inner)
    .await;
}

async fn write_cobs_sock<T: serde::Serialize>(
    mut write: tokio::net::unix::OwnedWriteHalf,
    stream: impl Stream<Item = T>,
    cancel: CancellationToken,
) {
    let auto_cancel = CancelDropGuard::from(cancel);
    async {
        use tokio::io::AsyncWriteExt as _;
        tokio::pin!(stream);
        while let Some(item) = stream.next().await {
            let Ok(buf) = postcard::to_stdvec_cobs(&item)
                .map_err(|err| log::error!("Failed to serialize update: {err}"))
            else {
                continue;
            };

            if let Err(err) = write.write_all(&buf).await {
                log::error!(
                    "Failed to write {} to socket: {err}",
                    std::any::type_name::<T>()
                );
                break;
            }
        }
    }
    .with_cancellation_token(&auto_cancel.inner)
    .await;
}

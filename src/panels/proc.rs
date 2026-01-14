use std::{
    ffi::{OsStr, OsString},
    time::Duration,
};

use anyhow::Context as _;
use futures::Stream;
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;
use tokio_stream::StreamExt as _;
use tokio_util::{sync::CancellationToken, time::FutureExt as _};

use crate::{
    tui,
    utils::{CancelDropGuard, Emit as _, ResultExt as _, SharedEmit, unb_chan},
};

const SOCK_PATH_VAR: &str = "BAR_TERM_INSTANCE_SOCK_PATH";
pub const PANEL_PROC_ARG: &str = "internal-managed-terminal";
pub const PROC_LOG_NAME_VAR: &str = "BAR_TERM_INSTANCE";

#[derive(Serialize, Deserialize, Debug)]
pub enum TermUpdate {
    Print(Vec<u8>),
    Flush,
    RemoteControl(Vec<OsString>),
    Shell(OsString, Vec<OsString>), // TODO: Envs
    Shutdown,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum TermEvent {
    Crossterm(crossterm::event::Event),
    Sizes(tui::Sizes),
}

pub fn spawn_generic_panel<AK: AsRef<OsStr>, AV: AsRef<OsStr>>(
    log_name: &str,
    upd_rx: impl Stream<Item = TermUpdate> + 'static + Send,
    extra_args: impl IntoIterator<Item: AsRef<OsStr>>,
    extra_envs: impl IntoIterator<Item = (AK, AV)>,
    term_ev_tx: impl SharedEmit<TermEvent>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let tmpdir = tempfile::TempDir::new()?;
    let sock_path = tmpdir.path().join("term-updates.sock");
    let socket = tokio::net::UnixListener::bind(&sock_path)?;
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
        .spawn()?;

    // TODO: Consider removing spawn?
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

        // Allow the manager to send the shutdown to the child before
        // deleting its socket and aborting its task.
        let child_res = child.wait().timeout(Duration::from_secs(10)).await;
        drop(mgr);
        drop(tmpdir);

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
    socket: tokio::net::UnixListener,
    ev_tx: impl SharedEmit<TermEvent>,
    updates: impl Stream<Item = TermUpdate> + Send + 'static,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let _auto_cancel = CancelDropGuard::from(cancel.clone());
    let mut tasks = JoinSet::<Option<()>>::new();
    // TODO: Await stream

    let (socket, _) = socket
        .accept()
        .timeout(Duration::from_secs(5))
        .await
        .context("Timed out while accepting socket connection")?
        .context("Failed to accept socket connection")?;
    let (read_half, write_half) = socket.into_split();

    tasks.spawn(
        read_cobs_sock::<TermEvent>(read_half, ev_tx, cancel.clone().drop_guard())
            .with_cancellation_token_owned(cancel.clone()),
    );
    tasks.spawn(
        write_cobs_sock::<TermUpdate>(write_half, updates, cancel.clone().drop_guard())
            .with_cancellation_token_owned(cancel.clone()),
    );

    if let Some(Err(err)) = tasks.join_next().await {
        log::error!("Error with task: {err}");
    }
    cancel.cancel();
    tasks.join_all().await;

    Ok(())
}

pub async fn term_proc_main() {
    term_proc_main_inner().await.ok_or_log();
}

async fn term_proc_main_inner() -> anyhow::Result<()> {
    let proc_tok = CancellationToken::new();
    let (mut ev_tx, upd_rx);
    {
        let socket = std::env::var_os(SOCK_PATH_VAR).context("Missing socket path env var")?;
        let socket = tokio::net::UnixStream::connect(socket)
            .await
            .context("Failed to connect to socket")?;
        let (read, write) = socket.into_split();

        let (upd_tx, ev_rx);
        (ev_tx, ev_rx) = unb_chan::<TermEvent>();
        (upd_tx, upd_rx) = std::sync::mpsc::channel::<TermUpdate>();

        tokio::spawn(read_cobs_sock(read, upd_tx, proc_tok.clone().drop_guard()));
        tokio::spawn(write_cobs_sock(write, ev_rx, proc_tok.clone().drop_guard()));
    }

    crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::EnterAlternateScreen,
        crossterm::cursor::Hide,
        crossterm::event::EnableMouseCapture,
    )?;
    crossterm::terminal::enable_raw_mode()?;

    let init_sizes = tui::Sizes::query()?;

    if let Err(err) = ev_tx.try_emit(TermEvent::Sizes(init_sizes)) {
        return Err(err).context("Failed to send initial font size while starting panel. Exiting.");
    }

    let proc_tok_clone = proc_tok.clone();
    tokio::spawn(async move {
        let _important_task = proc_tok_clone.drop_guard();
        let mut events = crossterm::event::EventStream::new();
        while let Some(ev) = events.next().await {
            match ev {
                Err(err) => log::error!("Crossterm error: {err}"),
                Ok(ev) => {
                    if let crossterm::event::Event::Resize(_, _) = &ev
                        && let Ok(sizes) = tui::Sizes::query().map_err(|err| log::error!("{err}"))
                    {
                        ev_tx.emit(TermEvent::Sizes(sizes));
                    }
                    ev_tx.emit(TermEvent::Crossterm(ev));
                }
            }
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

    let proc_tok_clone = proc_tok.clone();
    std::thread::spawn(move || {
        let _important_task = proc_tok_clone.drop_guard();

        use std::io::Write as _;
        let mut stdout = std::io::BufWriter::new(std::io::stdout().lock());
        while let Ok(upd) = upd_rx.recv() {
            match upd {
                TermUpdate::Shutdown => break,
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

    proc_tok.cancelled().await;

    Ok(())
}

async fn read_cobs_sock<T: serde::de::DeserializeOwned>(
    read: tokio::net::unix::OwnedReadHalf,
    mut tx: impl SharedEmit<T>,
    _auto_cancel: tokio_util::sync::DropGuard,
) {
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
                tx.emit(ev);
            }
        }

        buf.clear();
    }
}

async fn write_cobs_sock<T: serde::Serialize>(
    mut write: tokio::net::unix::OwnedWriteHalf,
    stream: impl Stream<Item = T>,
    _auto_cancel: tokio_util::sync::DropGuard,
) {
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

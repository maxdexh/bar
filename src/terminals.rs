use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    os::unix::ffi::OsStrExt,
    sync::Arc,
    time::Duration,
};

use anyhow::Context;
use futures::Stream;
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;
use tokio_stream::StreamExt as _;
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle, time::FutureExt as _};

use crate::{
    tui,
    utils::{Emit, ResultExt, SharedEmit, unb_chan, unb_rx_stream},
};

// TODO: Consider using uuids
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TermId(Arc<[u8]>);
impl TermId {
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
    pub fn from_bytes(s: &[u8]) -> Self {
        Self(s.into())
    }
}
impl std::fmt::Debug for TermId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut hasher = std::hash::DefaultHasher::new();
        std::hash::Hasher::write(&mut hasher, &self.0);
        let hash = std::hash::Hasher::finish(&hasher);
        f.debug_tuple("TermId").field(&hash).finish()
    }
}

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
#[derive(Debug)]
pub enum TermMgrUpdate {
    TermUpdate(TermId, TermUpdate),
    SpawnPanel(SpawnTerm),
}
#[derive(Debug)]
pub struct SpawnTerm {
    pub term_id: TermId,
    pub extra_args: Vec<OsString>,
    pub extra_envs: Vec<(OsString, OsString)>,
    pub term_ev_tx: tokio::sync::mpsc::UnboundedSender<TermEvent>,
}

pub const INTERNAL_ARG: &str = "internal-managed-terminal";

struct TermInst {
    upd_tx: tokio::sync::mpsc::UnboundedSender<TermUpdate>,
    cancel: CancellationToken,
}

pub async fn run_term_manager(updates: impl Stream<Item = TermMgrUpdate> + Send + 'static) {
    tokio::pin!(updates);

    let mut terminals = HashMap::new();

    let mut tasks = JoinSet::<()>::new();

    loop {
        let update = tokio::select! {
            Some(upd) = updates.next() => upd,
            Some(res) = tasks.join_next() => {
                if let Err(err) = res {
                    log::error!("Error with task: {err}");
                }
                continue
            },
            else => break,
        };
        match update {
            TermMgrUpdate::TermUpdate(tid, tupd) => {
                let Some(TermInst { upd_tx, cancel }) = terminals.get_mut(&tid) else {
                    log::error!(
                        "Cannot send update {tupd:?} to unknown terminal id {:?}",
                        tid
                    );
                    continue;
                };
                let is_shutdown = matches!(&tupd, TermUpdate::Shutdown);
                if upd_tx.emit(tupd).is_break() || is_shutdown {
                    cancel.cancel();
                    terminals.remove(&tid);
                }
            }
            TermMgrUpdate::SpawnPanel(spawn) => {
                let term_id = spawn.term_id.clone();

                let (upd_tx, upd_rx) = tokio::sync::mpsc::unbounded_channel();
                let cancel_inst = CancellationToken::new();
                if let Some(()) =
                    spawn_inst(spawn, unb_rx_stream(upd_rx), cancel_inst.clone()).ok_or_log()
                {
                    let old = terminals.insert(
                        term_id,
                        TermInst {
                            upd_tx,
                            cancel: cancel_inst,
                        },
                    );
                    if let Some(old) = old {
                        old.cancel.cancel();
                    }
                }
            }
        }
    }
}
fn spawn_inst(
    SpawnTerm {
        term_id,
        extra_args,
        extra_envs,
        term_ev_tx,
    }: SpawnTerm,
    upd_rx: impl Stream<Item = TermUpdate> + 'static + Send,
    inst_tok: CancellationToken,
) -> anyhow::Result<()> {
    let tmpdir = tempfile::tempdir()?;
    let sock_path = tmpdir.path().join("term-updates.sock");
    let socket = tokio::net::UnixListener::bind(&sock_path)?;
    let mut child = tokio::process::Command::new("kitten")
        .arg("panel")
        .args(extra_args)
        .arg(std::env::current_exe()?)
        .arg(INTERNAL_ARG)
        .envs(extra_envs)
        .env(SOCK_PATH_VAR, sock_path)
        .env(TERM_ID_VAR, OsStr::from_bytes(term_id.as_bytes()))
        .kill_on_drop(true)
        .stdout(std::io::stderr())
        .spawn()?;

    let mgr = AbortOnDropHandle::new({
        let inst_tok = inst_tok.clone();
        tokio::spawn(async move {
            run_term_inst_mgr(socket, term_ev_tx, upd_rx, inst_tok.clone())
                .await
                .context("Terminal instance failed")
                .ok_or_log();
            inst_tok.cancel();
        })
    });
    tokio::spawn(async move {
        tokio::select! {
            exit_res = child.wait() => {
                inst_tok.cancel();
                if let Err(err) = exit_res.context("Failed to wait for terminal exit") {
                    log::error!("{err:?}");
                    _ = child.kill().await;
                }
            }
            () = inst_tok.cancelled() => {
                let res = child.wait().timeout(Duration::from_secs(10)).await;
                let res: anyhow::Result<_> = (|| Ok(res??))();
                if let Err(err) = res.context("Terminal instance failed to exit after shutdown") {
                    log::error!("{err:?}");
                }
            }
        };
        mgr.abort();
        drop(tmpdir);
    });

    Ok(())
}

const SOCK_PATH_VAR: &str = "BAR_TERM_INSTANCE_SOCK_PATH";
pub const TERM_ID_VAR: &str = "BAR_TERM_INSTANCE_ID";

async fn run_term_inst_mgr(
    socket: tokio::net::UnixListener,
    ev_tx: impl SharedEmit<TermEvent>,
    updates: impl Stream<Item = TermUpdate> + Send + 'static,
    inst_cancel: CancellationToken,
) -> anyhow::Result<()> {
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
        read_cobs_sock::<TermEvent>(read_half, ev_tx, inst_cancel.clone())
            .with_cancellation_token_owned(inst_cancel.clone()),
    );
    tasks.spawn(
        write_cobs_sock::<TermUpdate>(write_half, updates, inst_cancel.clone())
            .with_cancellation_token_owned(inst_cancel.clone()),
    );

    if let Some(Err(err)) = tasks.join_next().await {
        log::error!("Error with task: {err}");
    }
    inst_cancel.cancel();
    tasks.join_all().await;

    Ok(())
}

pub async fn term_proc_main(term_id: TermId) {
    term_proc_main_inner(term_id).await.ok_or_log();
}

async fn term_proc_main_inner(term_id: TermId) -> anyhow::Result<()> {
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

        tokio::spawn(read_cobs_sock(read, upd_tx, proc_tok.clone()));
        tokio::spawn(write_cobs_sock(write, ev_rx, proc_tok.clone()));
    }

    crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::EnterAlternateScreen,
        crossterm::cursor::Hide,
        crossterm::event::EnableMouseCapture,
    )?;
    crossterm::terminal::enable_raw_mode()?;

    let init_sizes = tui::Sizes::query()?;

    if ev_tx.emit(TermEvent::Sizes(init_sizes)).is_break() {
        anyhow::bail!("Failed to send initial font size while starting {term_id:?}. Exiting.");
    }

    tokio::spawn(async move {
        let mut events = crossterm::event::EventStream::new();
        while let Some(ev) = events.next().await {
            match ev {
                Err(err) => log::error!("Crossterm error: {err}"),
                Ok(ev) => {
                    if let crossterm::event::Event::Resize(_, _) = &ev
                        && let Ok(sizes) = tui::Sizes::query().map_err(|err| log::error!("{err}"))
                    {
                        if ev_tx.emit(TermEvent::Sizes(sizes)).is_break() {
                            break;
                        }
                    }
                    if ev_tx.emit(TermEvent::Crossterm(ev)).is_break() {
                        break;
                    }
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
    std::thread::spawn(move || {
        use std::io::Write as _;
        let mut stdout = std::io::BufWriter::new(std::io::stdout().lock());
        while let Ok(upd) = upd_rx.recv() {
            match upd {
                TermUpdate::Shutdown => break,
                TermUpdate::Print(bytes) => {
                    if let Err(err) = stdout.write_all(&bytes) {
                        log::error!("Failed to write: {err}");
                    }
                }
                TermUpdate::Flush => {
                    if let Err(err) = stdout.flush() {
                        log::error!("Failed to flush: {err}");
                    }
                }
                TermUpdate::RemoteControl(args) => {
                    let Some(listen_on) = std::env::var_os("KITTY_LISTEN_ON") else {
                        log::error!("Missing KITTY_LISTEN_ON");
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
    on_disonnect: CancellationToken,
) {
    let _auto_cancel = on_disonnect.drop_guard();

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
                if tx.emit(ev).is_break() {
                    break;
                }
            }
        }

        buf.clear();
    }
}

async fn write_cobs_sock<T: Serialize>(
    mut write: tokio::net::unix::OwnedWriteHalf,
    stream: impl Stream<Item = T>,
    on_disonnect: CancellationToken,
) {
    let _auto_cancel = on_disonnect.drop_guard();

    use tokio::io::AsyncWriteExt as _;
    tokio::pin!(stream);
    while let Some(item) = stream.next().await {
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

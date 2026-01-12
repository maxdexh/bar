use std::{collections::HashMap, ffi::OsString, sync::Arc, time::Duration};

use anyhow::Context;
use futures::Stream;
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;
use tokio_stream::StreamExt as _;
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle, time::FutureExt as _};

use crate::{
    tui,
    utils::{CancelDropGuard, Emit, ResultExt, SharedEmit, UnbTx, unb_chan},
};

// TODO: Consider using uuids
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct TermId(Arc<str>);
impl TermId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn from_str(s: &str) -> Self {
        Self(s.into())
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
    // FIXME: Consider removing TermId and just using SpawnTerm fields
    pub term_id: TermId,
    pub extra_args: Vec<OsString>,
    pub extra_envs: Vec<(OsString, OsString)>,
    pub term_ev_tx: UnbTx<(TermId, TermEvent)>,
    pub cancel: CancellationToken,
}

pub const INTERNAL_ARG: &str = "internal-managed-terminal";

pub async fn run_term_manager(updates: impl Stream<Item = TermMgrUpdate> + Send + 'static) {
    tokio::pin!(updates);

    struct TermInst<E> {
        upd_tx: E,
        internal_id: u64,
        _auto_cancel: CancelDropGuard,
    }

    let mut next_internal_id = 0u64;
    let mut next_internal_id = || {
        next_internal_id = next_internal_id.checked_add(1).unwrap();
        next_internal_id
    };

    let mut terminals = HashMap::new();
    let (cancelled_tx, mut cancelled_rx) = unb_chan();

    loop {
        let update = tokio::select! {
            Some(update) = updates.next() => update,
            Some((term_id, iid)) = cancelled_rx.next() => {
                if terminals
                    .get(&term_id)
                    .is_some_and(|&TermInst { internal_id, .. }| iid == internal_id)
                {
                    terminals.remove(&term_id);
                }
                continue;
            }
        };
        match update {
            TermMgrUpdate::TermUpdate(tid, tupd) => {
                let Some(TermInst { upd_tx, .. }) = terminals
                    .get_mut(&tid)
                    .with_context(|| {
                        format!("Cannot send update {tupd:?} to unknown terminal id {tid:?}")
                    })
                    .ok_or_log()
                else {
                    continue;
                };
                let is_shutdown = matches!(&tupd, TermUpdate::Shutdown);
                Emit::emit(upd_tx, tupd);
                if is_shutdown {
                    terminals.remove(&tid);
                }
            }
            TermMgrUpdate::SpawnPanel(spawn) => {
                let internal_id = next_internal_id();
                let term_id = spawn.term_id.clone();

                let cancel = spawn.cancel.clone();
                let (upd_tx, upd_rx) = unb_chan();

                let res = spawn_inst(spawn, upd_rx, {
                    let term_id = term_id.clone();
                    cancelled_tx
                        .clone()
                        .with(move |()| (term_id.clone(), internal_id))
                });
                if res.ok_or_log().is_some() {
                    terminals.insert(
                        term_id,
                        TermInst {
                            upd_tx,
                            _auto_cancel: cancel.into(),
                            internal_id,
                        },
                    );
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
        cancel: inst_tok,
    }: SpawnTerm,
    upd_rx: impl Stream<Item = TermUpdate> + 'static + Send,
    mut on_exit: impl SharedEmit<()>,
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
        .env(TERM_ID_VAR, term_id.as_str())
        .kill_on_drop(true)
        .stdout(std::io::stderr())
        .spawn()?;

    tokio::spawn(async move {
        let mut mgr = AbortOnDropHandle::new(tokio::spawn(run_term_inst_mgr(
            socket,
            term_ev_tx.with(move |ev| (term_id.clone(), ev)),
            upd_rx,
            inst_tok.clone(),
        )));
        tokio::select! {
            exit_res = child.wait() => {
                exit_res.context("Failed to wait for terminal exit").ok_or_log();
            }
            () = inst_tok.cancelled() => {}
            run_res = &mut mgr => {
                run_res
                    .context("Terminal instance failed")
                    .ok_or_log();
            }
        };
        on_exit.emit(());
        inst_tok.cancel();

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
        read_cobs_sock::<TermEvent>(read_half, ev_tx, inst_cancel.clone().drop_guard())
            .with_cancellation_token_owned(inst_cancel.clone()),
    );
    tasks.spawn(
        write_cobs_sock::<TermUpdate>(write_half, updates, inst_cancel.clone().drop_guard())
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
        return Err(err).with_context(|| {
            format!("Failed to send initial font size while starting {term_id:?}. Exiting.")
        });
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

async fn write_cobs_sock<T: Serialize>(
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
            log::error!("Failed to write to update socket: {err}");
            break;
        }
    }
}

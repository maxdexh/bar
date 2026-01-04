use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    os::unix::ffi::OsStrExt,
    sync::Arc,
};

use serde::{Deserialize, Serialize};
use tokio::{sync::mpsc, task::JoinSet};
use tokio_stream::StreamExt as _;

use crate::{
    tui,
    utils::{Emit, SharedEmit, ubchan},
};

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
pub enum TermMgrUpdate<E> {
    TermUpdate(Vec<TermId>, TermUpdate),
    SpawnPanel(SpawnTerm<E>),
}
#[derive(Debug)]
pub struct SpawnTerm<E> {
    pub term_id: TermId,
    pub extra_args: Vec<OsString>,
    pub extra_envs: Vec<(OsString, OsString)>,
    pub event_tx: E,
}

pub const INTERNAL_ARG: &str = "internal-managed-terminal";

#[derive(Debug)]
enum MgrUpdMerge<E> {
    Mgr(TermMgrUpdate<E>),
    OnStop(TermId, u64),
}
pub async fn run_term_manager<E: SharedEmit<TermEvent>>(
    updates: impl futures::Stream<Item = TermMgrUpdate<E>> + Send + 'static,
) {
    tokio::pin!(updates);

    struct Term {
        upd_tx: mpsc::UnboundedSender<Arc<TermUpdate>>,
        internal_id: u64,
        child: tokio::process::Child,
        task: tokio::task::AbortHandle,
    }
    let mut terminals = HashMap::new();

    let mut tasks = JoinSet::new();

    let mut next_internal_id = 0u64;
    let mut next_internal_id = || {
        next_internal_id += 1;
        next_internal_id
    };

    loop {
        let update = tokio::select! {
            Some(upd) = updates.next() => MgrUpdMerge::Mgr(upd),
            Some(res) = tasks.join_next() => match res {
                Ok(upd) => upd,
                Err(err) => {
                    log::error!("Error joining task: {err}");
                    continue;
                }
            },
            else => return,
        };
        match update {
            MgrUpdMerge::Mgr(TermMgrUpdate::TermUpdate(tids, tupd)) => {
                let tupd = Arc::new(tupd);
                for tid in tids {
                    let Some(Term { upd_tx, .. }) = terminals.get(&tid) else {
                        log::error!("Unknown terminal id {:?}", tid);
                        continue;
                    };
                    if let TermUpdate::Shutdown = &*tupd {
                        // TODO: Timeout then kill
                    }
                    if let Err(err) = upd_tx.send(tupd.clone()) {
                        // TODO: Kill
                    }
                }
            }
            MgrUpdMerge::Mgr(TermMgrUpdate::SpawnPanel(SpawnTerm {
                term_id,
                extra_args,
                extra_envs,
                event_tx,
            })) => {
                let (upd_tx, mut upd_rx) = mpsc::unbounded_channel();

                let init_res = (|| {
                    let tmpdir = tempfile::tempdir()?;
                    let sock_path = tmpdir.path().join("term-updates.sock");
                    let listener = tokio::net::UnixListener::bind(&sock_path)?;
                    // TODO: Set stdout
                    let child = tokio::process::Command::new("kitten")
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
                    Ok::<_, anyhow::Error>((tmpdir, listener, child))
                })();
                let Ok((tmpdir, listener, child)) =
                    init_res.map_err(|err| log::error!("Failed to spawn panel: {err}"))
                else {
                    continue;
                };
                let internal_id = next_internal_id();

                let task = tasks.spawn({
                    let term_id = term_id.clone();
                    async move {
                        let res = run_term_controller(
                            tmpdir,
                            listener,
                            event_tx,
                            futures::stream::poll_fn(move |cx| upd_rx.poll_recv(cx)),
                        )
                        .await;

                        if let Err(err) = res {
                            log::error!("Terminal failed: {err}");
                        }

                        MgrUpdMerge::OnStop(term_id, internal_id)
                    }
                });
                if let Some(old) = terminals.insert(
                    term_id,
                    Term {
                        upd_tx,
                        internal_id,
                        child,
                        task,
                    },
                ) {
                    todo!()
                }
            }
            MgrUpdMerge::OnStop(term_id, internal_id) => todo!(),
        }
    }
}

const SOCK_PATH_VAR: &str = "BAR_TERM_INSTANCE_SOCK_PATH";
const TERM_ID_VAR: &str = "BAR_TERM_INSTANCE_ID";

async fn run_term_controller(
    _tmpdir_guard: tempfile::TempDir,
    listener: tokio::net::UnixListener,
    ev_tx: impl SharedEmit<TermEvent>,
    updates: impl futures::Stream<Item = Arc<TermUpdate>> + Send + 'static,
) -> anyhow::Result<()> {
    let mut tasks = JoinSet::new();
    // TODO: Await stream

    let (socket, _) = listener.accept().await?;
    let (read_half, write_half) = socket.into_split();

    tasks.spawn(read_cobs_sock::<TermEvent>(read_half, ev_tx));
    tasks.spawn(write_cobs_sock::<Arc<TermUpdate>>(write_half, updates));

    if let Some(Err(err)) = tasks.join_next().await {
        log::error!("Error with task: {err}");
    }

    Ok(())
}

pub async fn term_proc_main() -> anyhow::Result<()> {
    let Some(term_id) = std::env::var_os(TERM_ID_VAR) else {
        anyhow::bail!("Missing term id env var");
    };
    let term_id = TermId::from_bytes(term_id.as_bytes());
    log::info!("Term process {term_id:?} started");

    crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::EnterAlternateScreen,
        crossterm::cursor::Hide,
        crossterm::event::EnableMouseCapture,
    )?;
    crossterm::terminal::enable_raw_mode()?;

    let mut tasks = JoinSet::new();
    let (mut ev_tx, upd_rx);
    {
        let socket = std::env::var_os(SOCK_PATH_VAR).ok_or(std::env::VarError::NotPresent)?;
        let socket = tokio::net::UnixStream::connect(socket).await?;
        let (read, write) = socket.into_split();

        let (upd_tx, ev_rx);
        (ev_tx, ev_rx) = ubchan::<TermEvent>();
        (upd_tx, upd_rx) = std::sync::mpsc::channel::<TermUpdate>();
        tasks.spawn(read_cobs_sock(read, upd_tx));
        tasks.spawn(write_cobs_sock(write, ev_rx));
    }

    let init_sizes = tui::Sizes::query()?;
    if ev_tx.emit(TermEvent::Sizes(init_sizes)).is_break() {
        anyhow::bail!("Failed to send initial font size while starting {term_id:?}. Exiting.");
    }

    tasks.spawn(async move {
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

    if let Some(Err(err)) = tasks.join_next().await {
        log::error!("{err}");
    }

    Ok(())
}

async fn read_cobs_sock<T: serde::de::DeserializeOwned>(
    read: tokio::net::unix::OwnedReadHalf,
    mut tx: impl SharedEmit<T>,
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
    stream: impl futures::Stream<Item = T>,
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

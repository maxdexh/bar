mod ipc;

use bar_common::utils::{CancelDropGuard, ResultExt as _, UnbTx};

use std::ffi::OsString;
use std::{ffi::OsStr, path::Path, time::Duration};

use anyhow::Context as _;
use futures::Stream;
use tokio::task::JoinSet;
use tokio_util::{sync::CancellationToken, time::FutureExt as _};

pub use ipc::{TermEvent, TermUpdate};

// FIXME: Return channels instead of taking streams as args, also return
// initial sizes
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
        .arg("bar-proc-mgr")
        .envs(extra_envs)
        .env(ipc::SOCK_PATH_VAR, sock_path)
        .env(ipc::PROC_LOG_NAME_VAR, log_name)
        .env("PATH", std::env::var_os("PATH").unwrap())
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

    tasks.spawn(ipc::read_cobs_sock::<TermEvent>(
        read_half,
        move |x| {
            ev_tx.send(x).ok_or_debug();
        },
        cancel.clone(),
    ));
    tasks.spawn(ipc::write_cobs_sock::<TermUpdate>(
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

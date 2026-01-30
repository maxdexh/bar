mod ipc;

use anyhow::Context as _;
use bar_common::{
    tui,
    utils::{CancelDropGuard, ResultExt as _, unb_chan},
};
use futures::StreamExt as _;
use ipc::{TermEvent, TermUpdate};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

fn main() -> anyhow::Result<()> {
    let (log_name, res) =
        match std::env::var(ipc::PROC_LOG_NAME_VAR).context("Missing log name env var") {
            Ok(name) => (name, Ok(())),
            Err(err) => ("UNKNOWN".into(), Err(err)),
        };

    bar_common::logging::init_logger(bar_common::logging::ProcKind::Panel, log_name);

    res.ok_or_log();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("Failed to start the tokio runtime")
        .ok_or_log()
        .unwrap_or_else(|| std::process::exit(1));

    let main_handle = runtime.spawn(term_proc_main_inner());
    runtime
        .block_on(main_handle)
        .context("Failed to join main")?
}

async fn term_proc_main_inner() -> anyhow::Result<()> {
    let mut tasks = JoinSet::new();
    let cancel = CancellationToken::new();
    let (ev_tx, upd_rx);
    {
        let socket = std::env::var_os(ipc::SOCK_PATH_VAR).context("Missing socket path env var")?;
        let socket = tokio::net::UnixStream::connect(socket)
            .await
            .context("Failed to connect to socket")?;
        let (read, write) = socket.into_split();

        let (upd_tx, ev_rx);
        (ev_tx, ev_rx) = unb_chan::<TermEvent>();
        (upd_tx, upd_rx) = std::sync::mpsc::channel::<TermUpdate>();

        tasks.spawn(ipc::read_cobs_sock(
            read,
            move |x| {
                upd_tx.send(x).ok_or_debug();
            },
            cancel.clone(),
        ));
        tasks.spawn(ipc::write_cobs_sock(write, ev_rx, cancel.clone()));
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

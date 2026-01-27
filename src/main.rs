mod clients;
mod data;
mod logging;
mod monitors;
mod panels;
mod simple_bar;
mod tui;
mod utils;

fn main() -> std::process::ExitCode {
    use crate::{logging::ProcKind, utils::ResultExt as _};
    use anyhow::Context as _;

    crate::logging::init_logger();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("Failed to start the tokio runtime")
        .ok_or_log()
        .unwrap_or_else(|| std::process::exit(1));

    let signals_task = runtime.spawn(async move {
        type SK = tokio::signal::unix::SignalKind;

        let mut tasks = tokio::task::JoinSet::new();

        let (tx, mut rx) = tokio::sync::mpsc::channel(10);

        for kind in [
            SK::interrupt(),
            SK::quit(),
            SK::alarm(),
            SK::hangup(),
            SK::pipe(),
            SK::terminate(),
            SK::user_defined1(),
            SK::user_defined2(),
        ] {
            let Some(mut signal) = tokio::signal::unix::signal(kind).ok_or_log() else {
                continue;
            };
            let tx = tx.clone();
            tasks.spawn(async move {
                while let Some(()) = signal.recv().await
                    && tx.send(kind).await.is_ok()
                {}
            });
        }
        drop(tx);

        rx.recv()
            .await
            .context("Failed to receive any signals")
            .map(|kind| {
                let code = 128 + kind.as_raw_value();
                std::process::ExitCode::from(code as u8)
            })
            .ok_or_log()
    });
    let signals_task = async move {
        signals_task
            .await
            .context("Signal handler failed")
            .ok_or_log()
            .flatten()
    };

    let main_task =
        tokio_util::task::AbortOnDropHandle::new(match crate::logging::proc_kind_from_args() {
            ProcKind::Panel => runtime.spawn(crate::panels::proc::term_proc_main()),
            ProcKind::Controller => runtime.spawn(crate::simple_bar::main()),
        });

    runtime.block_on(async move {
        tokio::select! {
            res = main_task => match res.ok_or_log() {
                Some(code) => code,
                None => std::process::ExitCode::FAILURE,
            },
            Some(code) = signals_task => code,
        }
    })
}

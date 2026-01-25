mod clients;
mod data;
mod logging;
mod monitors;
mod panels;
mod simple_bar;
mod tui;
mod utils;

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    tokio::select! {
        () = main_inner() => {}
        () = exit_signal() => {}
    }
}

async fn main_inner() {
    use crate::logging::{ProcKindForLogger, init_logger};
    use crate::utils::ResultExt as _;

    let mut args = std::env::args().skip(1);
    use crate::panels::proc;
    match args.next().as_deref() {
        Some(proc::PANEL_PROC_ARG) => {
            let Some(name) = std::env::var(proc::PROC_LOG_NAME_VAR).ok_or_log() else {
                return;
            };
            log::info!("Term process {name:?} started");
            init_logger(ProcKindForLogger::Panel(name.clone()));
            crate::panels::proc::term_proc_main().await
        }
        None => {
            init_logger(ProcKindForLogger::Controller);

            simple_bar::main2().await
        }
        _ => log::error!("Bad arguments"),
    }
}

async fn exit_signal() {
    use tokio::signal::unix::SignalKind;
    let mut tasks = tokio::task::JoinSet::new();
    for kind in [
        SignalKind::interrupt(),
        SignalKind::quit(),
        SignalKind::alarm(),
        SignalKind::hangup(),
        SignalKind::pipe(),
        SignalKind::terminate(),
        SignalKind::user_defined1(),
        SignalKind::user_defined2(),
    ] {
        if let Some(mut signal) = utils::ResultExt::ok_or_log(tokio::signal::unix::signal(kind)) {
            tasks.spawn(async move {
                let exit = signal.recv().await.is_some();
                if exit {
                    log::debug!("Exiting for signal {kind:?}");
                }
                exit
            });
        }
    }
    loop {
        let Some(quit) = tasks.join_next().await else {
            return std::future::pending().await;
        };
        if utils::ResultExt::ok_or_log(quit) != Some(false) {
            break;
        }
    }
}

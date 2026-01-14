mod data;
mod logging;
mod modules;
mod monitors;
mod panels;
mod simple_bar;
mod tui;
mod utils;

#[tokio::main(flavor = "multi_thread")]
async fn main() {
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

            simple_bar::main().await
        }
        _ => log::error!("Bad arguments"),
    }
}

use std::os::unix::ffi::OsStrExt;

use anyhow::Context as _;

use crate::{
    logging::{ProcKindForLogger, init_logger},
    utils::ResultExt as _,
};

pub mod bar_panel;
pub mod controller;
pub mod menu_panel;

const EDGE: &str = "top";

pub async fn entry_point() {
    let mut args = std::env::args().skip(1);

    match args.next().as_deref() {
        Some(crate::terminals::INTERNAL_ARG) => {
            let Some(term_id) = std::env::var_os(crate::terminals::TERM_ID_VAR)
                .context("Missing term id env var")
                .ok_or_log()
            else {
                return;
            };
            let term_id = crate::terminals::TermId::from_bytes(term_id.as_bytes());
            log::info!("Term process {term_id:?} started");
            init_logger(ProcKindForLogger::Panel(term_id.clone()));
            crate::terminals::term_proc_main(term_id).await
        }
        None => {
            init_logger(ProcKindForLogger::Controller);

            controller::main().await
        }
        _ => log::error!("Bad arguments"),
    }
}

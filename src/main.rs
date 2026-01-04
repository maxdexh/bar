#![allow(clippy::collapsible_if)]

mod clients;
mod data;
mod logging;
mod procs;
mod terminals;
mod tui;
mod utils;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    procs::entry_point()
        .await
        .inspect_err(|err| log::error!("{err}"))
}

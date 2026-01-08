#![allow(clippy::collapsible_if)]

mod clients;
mod data;
mod logging;
mod panels;
mod procs;
mod terminals;
mod tui;
mod utils;

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    procs::entry_point().await
}

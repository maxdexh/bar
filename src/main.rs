mod clients;
mod data;
mod logging;
mod procs;
mod utils;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    procs::entry_point()
        .await
        .inspect_err(|err| log::error!("{err}"))
}

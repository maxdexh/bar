use std::ffi::OsString;

use bar_common::utils::CancelDropGuard;
use futures::{Stream, StreamExt as _};
use serde::{Deserialize, Serialize};
use tokio_util::{sync::CancellationToken, time::FutureExt as _};

pub(crate) const SOCK_PATH_VAR: &str = "BAR_TERM_INSTANCE_SOCK_PATH";
pub(crate) const PROC_LOG_NAME_VAR: &str = "BAR_TERM_INSTANCE_NAME";

#[derive(Serialize, Deserialize, Debug)]
#[non_exhaustive]
pub enum TermUpdate {
    Print(Vec<u8>),
    Flush,
    RemoteControl(Vec<OsString>),
    Shell(OsString, Vec<OsString>), // TODO: Envs
}

#[derive(Serialize, Deserialize, Debug)]
pub enum TermEvent {
    Crossterm(crossterm::event::Event),
    Sizes(bar_common::tui::Sizes),
    FocusChange { is_focused: bool },
}

pub(crate) async fn read_cobs_sock<T: serde::de::DeserializeOwned>(
    read: tokio::net::unix::OwnedReadHalf,
    tx: impl Fn(T),
    cancel: CancellationToken,
) {
    let auto_cancel = CancelDropGuard::from(cancel);
    async {
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
                    tx(ev);
                }
            }

            buf.clear();
        }
    }
    .with_cancellation_token(&auto_cancel.inner)
    .await;
}

pub(crate) async fn write_cobs_sock<T: serde::Serialize>(
    mut write: tokio::net::unix::OwnedWriteHalf,
    stream: impl Stream<Item = T>,
    cancel: CancellationToken,
) {
    let auto_cancel = CancelDropGuard::from(cancel);
    async {
        use tokio::io::AsyncWriteExt as _;
        tokio::pin!(stream);
        while let Some(item) = stream.next().await {
            let Ok(buf) = postcard::to_stdvec_cobs(&item)
                .map_err(|err| log::error!("Failed to serialize update: {err}"))
            else {
                continue;
            };

            if let Err(err) = write.write_all(&buf).await {
                log::error!(
                    "Failed to write {} to socket: {err}",
                    std::any::type_name::<T>()
                );
                break;
            }
        }
    }
    .with_cancellation_token(&auto_cancel.inner)
    .await;
}

use futures::Stream;
use tokio::sync::broadcast;

#[track_caller]
pub fn lossy_broadcast<T: Clone>(rx: broadcast::Receiver<T>) -> impl Stream<Item = T> {
    broadcast_stream(rx, |n| {
        log::warn!(
            "Lagged {n} items on lossy stream ({})",
            std::any::type_name::<T>()
        );
        None
    })
}
pub fn broadcast_stream<T: Clone>(
    mut rx: broadcast::Receiver<T>,
    mut on_lag: impl FnMut(u64) -> Option<T>,
) -> impl Stream<Item = T> {
    stream_from_fn(async move || {
        loop {
            match rx.recv().await {
                Ok(value) => break Some(value),
                Err(broadcast::error::RecvError::Closed) => break None,
                Err(broadcast::error::RecvError::Lagged(n)) => match on_lag(n) {
                    Some(item) => break Some(item),
                    None => continue,
                },
            }
        }
    })
}
pub fn stream_from_fn<T>(f: impl AsyncFnMut() -> Option<T>) -> impl Stream<Item = T> {
    tokio_stream::StreamExt::filter_map(
        futures::stream::unfold(f, |mut f| async move { Some((f().await, f)) }),
        |it| it,
    )
}

pub type WatchTx<T> = tokio::sync::watch::Sender<T>;
pub type WatchRx<T> = tokio::sync::watch::Receiver<T>;
pub fn watch_chan<T>(init: T) -> (WatchTx<T>, WatchRx<T>) {
    tokio::sync::watch::channel(init)
}

pub type UnbTx<T> = tokio::sync::mpsc::UnboundedSender<T>;
pub type UnbRx<T> = tokio_stream::wrappers::UnboundedReceiverStream<T>;
pub fn unb_chan<T>() -> (UnbTx<T>, UnbRx<T>) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    (tx, rx.into())
}

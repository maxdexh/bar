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
pub struct UnbRx<T> {
    pub inner: tokio::sync::mpsc::UnboundedReceiver<T>,
}
const _: () = {
    use tokio::sync::mpsc::UnboundedReceiver;

    impl<T> futures::Stream for UnbRx<T> {
        type Item = T;

        fn poll_next(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            self.inner.poll_recv(cx)
        }

        fn size_hint(&self) -> (usize, Option<usize>) {
            (self.inner.len(), None)
        }
    }

    impl<T> From<UnboundedReceiver<T>> for UnbRx<T> {
        fn from(rx: UnboundedReceiver<T>) -> Self {
            Self { inner: rx }
        }
    }
};
pub fn unb_chan<T>() -> (UnbTx<T>, UnbRx<T>) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    (tx, rx.into())
}

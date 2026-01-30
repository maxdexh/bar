// TODO: Find a safer version of watch::Sender that is not prone to blocking and deadlocks
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

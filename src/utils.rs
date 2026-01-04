use std::{borrow::Borrow, collections::HashMap, sync::Arc};

use futures::Stream;
use tokio::sync::broadcast;

#[track_caller]
pub fn broadcast_stream<T: Clone>(rx: broadcast::Receiver<T>) -> impl Stream<Item = T> {
    broadcast_stream_base(rx, |n| {
        log::warn!(
            "Lagged {n} items on lossy stream ({})",
            std::any::type_name::<T>()
        );
        None
    })
}
pub fn broadcast_stream_base<T: Clone>(
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

struct BasicTaskMapInner<K, T> {
    tasks: HashMap<K, tokio::task::JoinHandle<T>>,
}
impl<K, T> Default for BasicTaskMapInner<K, T> {
    fn default() -> Self {
        Self {
            tasks: Default::default(),
        }
    }
}
impl<K, T> Drop for BasicTaskMapInner<K, T> {
    fn drop(&mut self) {
        for (_, handle) in self.tasks.drain() {
            handle.abort();
        }
    }
}
pub struct BasicTaskMap<K, T> {
    inner: Arc<std::sync::Mutex<BasicTaskMapInner<K, T>>>,
}
impl<K, T> Clone for BasicTaskMap<K, T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}
impl<K, T> BasicTaskMap<K, T> {
    pub fn new() -> Self {
        Self {
            inner: Default::default(),
        }
    }

    pub fn insert_spawn<Fut>(&self, key: K, fut: Fut)
    where
        K: std::hash::Hash + Eq + Send + 'static + Clone,
        T: Send + 'static,
        Fut: Future<Output = T> + Send + 'static,
    {
        let weak = Arc::downgrade(&self.inner);

        let key_clone = key.clone();
        let handle = tokio::spawn(async move {
            let res = fut.await;
            if let Some(tasks) = weak.upgrade()
                && let Some(id) = tokio::task::try_id()
            {
                let mut inner = tasks.lock().unwrap_or_else(|poi| poi.into_inner());
                if inner.tasks.get(&key_clone).is_some_and(|it| it.id() == id) {
                    inner.tasks.remove(&key_clone);
                }
            }
            res
        });

        let mut inner = self.inner.lock().unwrap_or_else(|poi| poi.into_inner());
        if let Some(handle) = inner.tasks.insert(key, handle) {
            handle.abort();
        }
    }

    pub fn cancel<Q>(&self, key: &Q)
    where
        K: Borrow<Q> + std::hash::Hash + Eq,
        Q: std::hash::Hash + Eq + ?Sized,
    {
        let mut inner = self.inner.lock().unwrap_or_else(|poi| poi.into_inner());
        if let Some(handle) = inner.tasks.remove(key) {
            handle.abort();
        }
    }
}

pub struct ReloadRx {
    rx: broadcast::Receiver<()>,
}
impl ReloadRx {
    // TODO: Return ControlFlow
    pub fn blocking_wait(&mut self) -> Option<()> {
        match self.rx.blocking_recv() {
            Ok(()) | Err(broadcast::error::RecvError::Lagged(_)) => Some(()),
            _ => None,
        }
    }
    pub async fn wait(&mut self) -> Option<()> {
        // if we get Err(Lagged) or Ok, it means a reload request was issued.
        match self.rx.recv().await {
            Ok(()) | Err(broadcast::error::RecvError::Lagged(_)) => Some(()),
            _ => None,
        }
    }
    pub fn into_stream(self) -> impl Stream<Item = ()> {
        broadcast_stream_base(self.rx, |_| Some(()))
    }

    // TODO: debounce/rate limit this heavily: after accepting a reload request, merge all
    // requests from the next few seconds into one.
    // TODO: Cause extra reloads from time to time.
    pub fn new() -> (impl Clone + Fn(), Self) {
        let (tx, rx) = broadcast::channel(1);
        (
            move || {
                _ = tx.send(());
            },
            Self { rx },
        )
    }

    pub fn resubscribe(&self) -> Self {
        Self {
            rx: self.rx.resubscribe(),
        }
    }
}

pub fn unb_rx_stream<T>(mut rx: tokio::sync::mpsc::UnboundedReceiver<T>) -> impl Stream<Item = T> {
    futures::stream::poll_fn(move |cx| rx.poll_recv(cx))
}
pub fn ubchan<T>() -> (impl Emit<T> + Clone, impl Stream<Item = T>) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    (tx, unb_rx_stream(rx))
}

pub trait Emit<T> {
    #[track_caller]
    fn emit(&mut self, value: T) -> std::ops::ControlFlow<()>;
}
pub async fn dump_stream<T>(mut emit: impl Emit<T>, stream: impl Stream<Item = T>) {
    tokio::pin!(stream);
    while let Some(item) = tokio_stream::StreamExt::next(&mut stream).await {
        if emit.emit(item).is_break() {
            break;
        }
    }
}
impl<T, F: FnMut(T) -> std::ops::ControlFlow<()>> Emit<T> for F {
    #[track_caller]
    fn emit(&mut self, value: T) -> std::ops::ControlFlow<()> {
        self(value)
    }
}
#[track_caller]
fn handle_sender_res<E: std::fmt::Display>(res: Result<(), E>) -> std::ops::ControlFlow<()> {
    match res {
        Ok(()) => std::ops::ControlFlow::Continue(()),
        Err(err) => {
            log::debug!("Failed to send: {err}");
            std::ops::ControlFlow::Break(())
        }
    }
}
impl<T> Emit<T> for tokio::sync::mpsc::UnboundedSender<T> {
    #[track_caller]
    fn emit(&mut self, value: T) -> std::ops::ControlFlow<()> {
        handle_sender_res(self.send(value))
    }
}
impl<T> Emit<T> for std::sync::mpsc::Sender<T> {
    #[track_caller]
    fn emit(&mut self, value: T) -> std::ops::ControlFlow<()> {
        handle_sender_res(self.send(value))
    }
}
pub trait SharedEmit<T>: Emit<T> + Clone + 'static + Send {}
impl<S: Emit<T> + Clone + 'static + Send, T> SharedEmit<T> for S {}
// pub fn fused_broadcast_sink
// pub fn patient_broadcast_sink

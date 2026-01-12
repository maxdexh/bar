use std::marker::PhantomData;

use futures::Stream;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

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

pub struct ReloadRx {
    // FIXME: tokio::sync::Notify
    rx: broadcast::Receiver<()>,
    //last_reload: Option<std::time::Instant>,
    //min_delay: std::time::Duration,
}
impl ReloadRx {
    // FIXME: Return Result
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
    pub fn into_stream(mut self) -> impl Stream<Item = ()> {
        stream_from_fn(async move || self.wait().await)
    }

    pub fn resubscribe(&self) -> Self {
        Self {
            rx: self.rx.resubscribe(),
        }
    }
}
#[derive(Clone)]
pub struct ReloadTx {
    tx: broadcast::Sender<()>,
}
impl ReloadTx {
    pub fn new() -> Self {
        Self {
            tx: broadcast::Sender::new(1),
        }
    }
    pub fn reload(&mut self) {
        _ = self.tx.send(());
    }
    pub fn subscribe(&self) -> ReloadRx {
        ReloadRx {
            rx: self.tx.subscribe(),
        }
    }
}

pub fn unb_chan<T>() -> (UnbTx<T>, UnbRx<T>) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    (tx, UnbRx::new(rx))
}

#[derive(Debug)]
pub struct EmitError<T>(PhantomData<T>);
impl<T> std::fmt::Display for EmitError<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "No receivers were available to receive {}",
            std::any::type_name::<T>()
        )
    }
}
impl<T: std::fmt::Debug> std::error::Error for EmitError<T> {}
impl<T> EmitError<T> {
    pub fn retype<U>(self) -> EmitError<U> {
        EmitError(PhantomData)
    }
}

pub type EmitResult<T> = Result<(), EmitError<T>>;

pub trait Emit<T> {
    #[track_caller]
    fn try_emit(&mut self, val: T) -> EmitResult<T>;

    #[track_caller]
    fn emit(&mut self, val: T) {
        if let Err(err) = self.try_emit(val) {
            log::warn!("{err}");
        }
    }

    fn with<U, F: FnMut(U) -> T>(self, f: F) -> EmitWith<Self, F, U>
    where
        Self: Sized,
    {
        EmitWith(self, f, PhantomData)
    }
}
impl<T, F: FnMut(T) -> EmitResult<T>> Emit<T> for F {
    fn try_emit(&mut self, val: T) -> EmitResult<T> {
        self(val)
    }
}
impl<T> Emit<T> for UnbTx<T> {
    fn try_emit(&mut self, val: T) -> EmitResult<T> {
        self.send(val).map_err(|_| EmitError(PhantomData))
    }
}
impl<T> Emit<T> for &UnbTx<T> {
    fn try_emit(&mut self, val: T) -> EmitResult<T> {
        self.send(val).map_err(|_| EmitError(PhantomData))
    }
}
impl<T> Emit<T> for std::sync::mpsc::Sender<T> {
    fn try_emit(&mut self, val: T) -> EmitResult<T> {
        self.send(val).map_err(|_| EmitError(PhantomData))
    }
}
impl<T> Emit<T> for &WatchTx<T> {
    fn try_emit(&mut self, val: T) -> EmitResult<T> {
        self.send(val).map_err(|_| EmitError(PhantomData))
    }
}
impl<T> Emit<T> for WatchTx<T> {
    fn try_emit(&mut self, val: T) -> EmitResult<T> {
        self.send(val).map_err(|_| EmitError(PhantomData))
    }
}
pub trait SharedEmit<T>: Emit<T> + Clone + 'static + Send + Sync {}
impl<S: Emit<T> + Clone + 'static + Send + Sync, T> SharedEmit<T> for S {}

pub struct EmitWith<E, F, U>(E, F, PhantomData<fn(U)>);
impl<E, F, T, U> Emit<U> for EmitWith<E, F, U>
where
    E: Emit<T>,
    F: FnMut(U) -> T,
{
    fn try_emit(&mut self, val: U) -> EmitResult<U> {
        self.0
            .try_emit(self.1(val))
            .map_err(|_| EmitError(PhantomData))
    }
}
impl<E: Clone, F: Clone, U> Clone for EmitWith<E, F, U> {
    fn clone(&self) -> Self {
        let Self(e, f, ..) = self;
        Self(e.clone(), f.clone(), PhantomData)
    }
}

pub trait ResultExt {
    type Ok;
    #[track_caller]
    fn ok_or_log(self) -> Option<Self::Ok>;
}
impl<T, E: Into<anyhow::Error>> ResultExt for Result<T, E> {
    type Ok = T;
    #[track_caller]
    fn ok_or_log(self) -> Option<T> {
        match self {
            Ok(val) => Some(val),
            Err(err) => {
                log::error!("{:?}", err.into());
                None
            }
        }
    }
}

pub type WatchTx<T> = tokio::sync::watch::Sender<T>;
pub type WatchRx<T> = tokio::sync::watch::Receiver<T>;
pub type UnbTx<T> = tokio::sync::mpsc::UnboundedSender<T>;
pub type UnbRx<T> = tokio_stream::wrappers::UnboundedReceiverStream<T>;
pub fn watch_chan<T>(init: T) -> (WatchTx<T>, WatchRx<T>) {
    tokio::sync::watch::channel(init)
}

pub struct CancelDropGuard {
    pub inner: CancellationToken,
}
impl Drop for CancelDropGuard {
    fn drop(&mut self) {
        self.inner.cancel();
    }
}
impl CancelDropGuard {
    pub fn disarm(self) -> CancellationToken {
        let disarm = std::mem::ManuallyDrop::new(self);
        // SAFETY: `disarm` is not used after this, including by drop impls,
        // so this copy is effectively a move out of the field.
        unsafe { std::ptr::read(&disarm.inner) }
    }
}
impl From<CancellationToken> for CancelDropGuard {
    fn from(inner: CancellationToken) -> Self {
        //tokio::sync::watch::Receiver::changed;
        //tokio::sync::watch::Sender::send;
        Self { inner }
    }
}

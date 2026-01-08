use std::ops::ControlFlow;

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
        broadcast_stream(self.rx, |_| Some(()))
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
pub fn unb_chan<T>() -> (impl Emit<T> + Clone, impl Stream<Item = T>) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    (tx, unb_rx_stream(rx))
}

pub trait Emit<T> {
    #[track_caller]
    fn emit(&mut self, val: T) -> ControlFlow<()>;
}
pub async fn dump_stream<T>(mut emit: impl Emit<T>, stream: impl Stream<Item = T>) {
    tokio::pin!(stream);
    while let Some(item) = tokio_stream::StreamExt::next(&mut stream).await {
        if emit.emit(item).is_break() {
            break;
        }
    }
}
impl<T, F: FnMut(T) -> ControlFlow<()>> Emit<T> for F {
    #[track_caller]
    fn emit(&mut self, val: T) -> ControlFlow<()> {
        self(val)
    }
}
#[track_caller]
fn handle_sender_res<E: std::fmt::Display>(res: Result<(), E>) -> ControlFlow<()> {
    match res {
        Ok(()) => ControlFlow::Continue(()),
        Err(err) => {
            log::warn!("Failed to send: {err}");
            ControlFlow::Break(())
        }
    }
}
impl<T> Emit<T> for tokio::sync::mpsc::UnboundedSender<T> {
    #[track_caller]
    fn emit(&mut self, val: T) -> ControlFlow<()> {
        handle_sender_res(self.send(val))
    }
}
impl<T> Emit<T> for std::sync::mpsc::Sender<T> {
    #[track_caller]
    fn emit(&mut self, val: T) -> ControlFlow<()> {
        handle_sender_res(self.send(val))
    }
}
impl<T> Emit<T> for tokio::sync::broadcast::Sender<T> {
    #[track_caller]
    fn emit(&mut self, val: T) -> ControlFlow<()> {
        handle_sender_res(self.send(val).map(|_| ()))
    }
}
pub trait SharedEmit<T>: Emit<T> + Clone + 'static + Send {}
impl<S: Emit<T> + Clone + 'static + Send, T> SharedEmit<T> for S {}

mod dyn_shared_emit {
    use super::*;

    pub trait IDynSharedEmit<T>: Emit<T> + 'static + Send {
        fn clone_dyn(&self) -> Box<dyn IDynSharedEmit<T>>;
    }
    impl<T, E: SharedEmit<T>> IDynSharedEmit<T> for E {
        fn clone_dyn(&self) -> Box<dyn IDynSharedEmit<T>> {
            Box::new(self.clone())
        }
    }
    pub struct DynSharedEmit<T>(Box<dyn IDynSharedEmit<T>>);
    impl<T: 'static> Clone for DynSharedEmit<T> {
        fn clone(&self) -> Self {
            Self(self.0.clone_dyn())
        }
    }
    impl<T> Emit<T> for DynSharedEmit<T> {
        fn emit(&mut self, val: T) -> ControlFlow<()> {
            self.0.emit(val)
        }
    }
}
pub use dyn_shared_emit::DynSharedEmit;

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

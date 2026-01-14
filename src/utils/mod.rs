use futures::FutureExt;
use tokio_util::sync::CancellationToken;

mod task;
pub use task::*;
mod reload;
pub use reload::*;
mod emit;
pub use emit::*;
mod channels;
pub use channels::*;

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

pub struct CancelDropGuard {
    pub inner: CancellationToken,
}
impl Drop for CancelDropGuard {
    fn drop(&mut self) {
        self.inner.cancel();
    }
}
impl From<CancellationToken> for CancelDropGuard {
    fn from(inner: CancellationToken) -> Self {
        //tokio::sync::watch::Receiver::changed;
        //tokio::sync::watch::Sender::send;
        Self { inner }
    }
}

macro_rules! await_first {
    ($($fut:ident),* $(,)?) => {{
        tokio::pin!($($fut),*);
        std::future::poll_fn(|cx| {
            $(
                if let ready @ std::task::Poll::Ready(_) = std::future::Future::poll($fut.as_mut(), cx) {
                    return ready;
                }
            )*
            std::task::Poll::Pending
        }).await
    }};
}
pub(crate) use await_first;

use std::borrow::Borrow;

use futures::Stream;
use tokio::sync::broadcast;

use crate::data::Location;

pub fn rect_center(
    rect: ratatui::layout::Rect,
    (font_w, font_h): ratatui_image::FontSize,
) -> Location {
    let font_w = u32::from(font_w);
    let font_h = u32::from(font_h);
    Location {
        x: u32::from(rect.x) * font_w + u32::from(rect.width) * font_w / 2,
        y: u32::from(rect.y) * font_h + u32::from(rect.height) * font_h / 2,
    }
}

pub trait ResultExt {
    type Ok;
    type Err;

    #[track_caller]
    fn ok_or_log(self, preface: &str) -> Option<Self::Ok>
    where
        Self::Err: Into<anyhow::Error>;
}
impl<T, E> ResultExt for Result<T, E> {
    type Ok = T;
    type Err = E;

    #[track_caller]
    fn ok_or_log(self, preface: &str) -> Option<T>
    where
        E: Into<anyhow::Error>,
    {
        #[track_caller]
        #[cold]
        fn do_log(preface: &str, err: anyhow::Error) {
            log::error!("{preface}: {err}");
        }

        match self {
            Err(err) => {
                do_log(preface, err.into());
                None
            }
            Ok(it) => Some(it),
        }
    }
}

// TODO: Consider using a UnixStream directly and sending COBS over it
#[repr(transparent)]
pub struct IpcSender<T: ?Sized> {
    raw: tokio_unix_ipc::RawSender,
    _p: std::marker::PhantomData<fn(std::marker::PhantomData<T>)>,
}
impl<T: serde::Serialize + ?Sized> IpcSender<T> {
    pub fn send_or_log(&self, value: impl Borrow<T>) -> impl Future<Output = ()> {
        let log_err = |err| log::error!("Error sending value: {err}");

        async move {
            if let Err(err) = self.send(value).await {
                log_err(err)
            }
        }
    }

    pub async fn send(&self, value: impl Borrow<T>) -> anyhow::Result<()> {
        self.send_any::<T>(value.borrow()).await
    }
}
impl<T: ?Sized> IpcSender<T> {
    fn new(raw: tokio_unix_ipc::RawSender) -> Self {
        IpcSender {
            raw,
            _p: std::marker::PhantomData,
        }
    }

    pub async fn send_any<U: serde::Serialize + ?Sized>(&self, value: &U) -> anyhow::Result<()> {
        let buf = postcard::to_stdvec(&value)?;
        assert_ne!(buf.len(), 0);
        self.raw.send(&buf, &[]).await?;
        Ok(())
    }
}

pub struct IpcReceiver<T: ?Sized> {
    raw: tokio_unix_ipc::RawReceiver,
    _p: std::marker::PhantomData<fn() -> std::marker::PhantomData<T>>,
}
impl<T> IpcReceiver<T> {
    pub fn into_stream(self) -> impl Stream<Item = T>
    where
        T: serde::de::DeserializeOwned + Send + 'static + std::fmt::Debug,
    {
        use tokio::sync::mpsc;

        // Use an unbounded channel and a dedicated task to buffer these.
        // This minimizes the time that messages spend in the backing
        // UnixStream and ensures we don't accidentally deadlock from two
        // processes waiting for each other to empty out a stream.
        //
        // It also means we don't deserialize on listener's task.
        //
        // TODO: Consider writing a dedicated stream impl
        let (helper_tx, helper_rx) = mpsc::unbounded_channel();
        tokio::task::spawn(async move {
            let self_name = std::any::type_name::<Self>();
            loop {
                match self.raw.recv().await {
                    Ok((buf, fds)) => {
                        assert_eq!(fds, None);
                        let Some(val) = postcard::from_bytes(&buf).ok_or_log(self_name) else {
                            continue;
                        };
                        if let Err(err) = helper_tx.send(val) {
                            log::error!("{self_name}: {err}");
                        }
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                        log::warn!("{self_name} hit EOF: {err}");
                        break;
                    }
                    Err(err) => {
                        log::error!("{self_name}: {err}");
                        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                    }
                };
            }
        });
        tokio_stream::wrappers::UnboundedReceiverStream::new(helper_rx)
    }
}
impl<T: ?Sized> IpcReceiver<T> {
    fn new(raw: tokio_unix_ipc::RawReceiver) -> Self {
        IpcReceiver {
            raw,
            _p: std::marker::PhantomData,
        }
    }
}

// TODO: Consider passing path via env vars instead
#[allow(clippy::complexity)]
pub fn bootstrap_panel<T, S: ?Sized, R, F: AsyncFnOnce(&std::path::Path) -> anyhow::Result<T>>() -> (
    impl AsyncFnOnce(F) -> anyhow::Result<(T, IpcSender<S>, IpcReceiver<R>)>,
    impl AsyncFnOnce(&str) -> anyhow::Result<(IpcSender<R>, IpcReceiver<S>)>,
) {
    // TODO: timeout
    (
        async |spawn| {
            let bootstrap_tx = tokio_unix_ipc::Bootstrapper::new()?;
            let child = spawn(bootstrap_tx.path()).await?;
            let (panel_tx, contr_rx) = tokio_unix_ipc::raw_channel()?;
            let (contr_tx, panel_rx) = tokio_unix_ipc::raw_channel()?;
            let (ready_tx, ready_rx) = tokio_unix_ipc::symmetric_channel()?;
            bootstrap_tx.send((contr_tx, contr_rx, ready_tx)).await?;
            let () = ready_rx.recv().await?;
            Ok((child, IpcSender::new(panel_tx), IpcReceiver::new(panel_rx)))
        },
        async |path| {
            let bootstrap_rx = tokio_unix_ipc::Receiver::connect(path).await?;
            let (contr_tx, contr_rx, ready_tx) = bootstrap_rx.recv().await?;
            tokio_unix_ipc::Sender::send(&ready_tx, ()).await?;
            Ok((IpcSender::new(contr_tx), IpcReceiver::new(contr_rx)))
        },
    )
}

#[track_caller]
pub fn fused_lossy_stream<T: Clone>(rx: broadcast::Receiver<T>) -> impl Stream<Item = T> {
    let on_lag = |n| {
        log::warn!(
            "Lagged {n} items on lossy stream ({})",
            std::any::type_name::<T>()
        )
    };
    let base = futures::stream::unfold(rx, move |mut rx| async move {
        let t = rx.recv().await;
        match t {
            Ok(value) => Some((Some(value), rx)),
            Err(broadcast::error::RecvError::Closed) => None,
            Err(broadcast::error::RecvError::Lagged(n)) => {
                on_lag(n);
                Some((None, rx))
            }
        }
    });
    tokio_stream::StreamExt::filter_map(base, |it| it)
}

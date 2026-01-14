use std::sync::Arc;

use tokio_util::task::AbortOnDropHandle;

type LazyInner<B> = (B, AbortOnDropHandle<()>);
pub struct LazyTask<B> {
    inner: tokio::sync::Mutex<std::sync::Weak<LazyInner<B>>>,
}
pub struct LazyTaskHandle<B>(Arc<LazyInner<B>>);
impl<B> LazyTaskHandle<B> {
    pub fn backend(&self) -> &B {
        &self.0.0
    }
}

impl<B> LazyTask<B> {
    pub fn new() -> Self {
        Self {
            inner: Default::default(),
        }
    }

    pub async fn enter<F: Future<Output = ()> + 'static + Send>(
        &self,
        init: impl AsyncFnOnce() -> (F, B),
    ) -> LazyTaskHandle<B> {
        let mut lock = self.inner.lock().await;
        LazyTaskHandle(if let Some(inst) = lock.upgrade() {
            inst
        } else {
            let (run_future, backend) = init().await;
            let task = AbortOnDropHandle::new(tokio::spawn(run_future));
            let inst = Arc::new((backend, task));
            *lock = Arc::downgrade(&inst);
            inst
        })
    }
}

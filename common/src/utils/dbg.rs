use std::{fmt, sync::Arc};

#[derive(Clone)]
pub struct Dbg<T> {
    pub inner: T,
    #[cfg(debug_assertions)]
    dbg: (&'static str, &'static std::panic::Location<'static>),
}
impl<T> fmt::Debug for Dbg<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut dbg = f.debug_tuple(std::any::type_name::<Self>());

        #[cfg(debug_assertions)]
        dbg.field(&fmt::from_fn(|f| {
            let (original_type, fn_location) = self.dbg;
            write!(f, "{original_type} @ {fn_location}")
        }));

        dbg.finish()
    }
}
impl<T> Dbg<T> {
    pub fn new<Original>(original: Original, to_inner: impl FnOnce(Original) -> T) -> Self {
        Self {
            inner: to_inner(original),
            #[cfg(debug_assertions)]
            dbg: (
                std::any::type_name::<Original>(),
                std::panic::Location::caller(),
            ),
        }
    }
}

pub struct Callback<T, R>(Dbg<Arc<dyn Fn(T) -> R + 'static + Send + Sync>>);
impl<T, R> Clone for Callback<T, R> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl<F, T, R> From<F> for Callback<T, R>
where
    F: Fn(T) -> R + 'static + Send + Sync,
{
    #[inline]
    #[track_caller]
    fn from(value: F) -> Self {
        Self::from_fn(value)
    }
}

impl<T, R> fmt::Debug for Callback<T, R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}
impl<T, R> Callback<T, R> {
    #[inline]
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn from_fn(callback: impl Fn(T) -> R + 'static + Send + Sync) -> Self {
        Self(Dbg::new(callback, |cb| Arc::new(cb) as _))
    }

    #[inline]
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn from_fn_ctx<C: 'static + Send + Sync>(
        ctx: C,
        callback: impl Fn(&C, T) -> R + 'static + Send + Sync,
    ) -> Self {
        Self(Dbg::new(callback, |cb| {
            Arc::new(move |arg| cb(&ctx, arg)) as _
        }))
    }

    pub fn call(&self, arg: T) -> R {
        (self.0.inner)(arg)
    }
}
impl<T, R> From<&Self> for Callback<T, R> {
    fn from(value: &Self) -> Self {
        value.clone()
    }
}

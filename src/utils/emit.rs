use std::marker::PhantomData;

use crate::utils::{UnbTx, WatchTx};

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

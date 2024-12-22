//! Utilities for woring with futures

use core::future::Future;

use std::cell::RefCell;

use alloc::sync::Arc;

use anyhow::Context;

use embassy_futures::join::{Join, Join3, Join4, Join5};
use embassy_futures::select::{Either, Either3, Either4, Select, Select3, Select4};

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;

use log::error;

extern crate alloc;

#[allow(unused)]
pub trait IntoUnitFallibleFuture {
    /// Convert a failible future into a fallible future that returns `()`
    async fn into_unit<T, E>(self) -> Result<(), E>
    where
        Self: Sized + Future<Output = Result<T, E>>,
    {
        self.await;
        Ok(())
    }
}

impl<T> IntoUnitFallibleFuture for T where T: Future {}

#[allow(unused)]
pub trait IntoUnitFuture {
    /// Convert a future into a future that returns `()`
    async fn into_unit(self)
    where
        Self: Sized + Future,
    {
        self.await;
    }
}

impl<T> IntoUnitFuture for T where T: Future {}

/// A trait for coalescing the outputs of `embassy_futures::Select*` and `embassy_futures::Join*` futures.
///
/// - The outputs of the `embassy_futures::Select*` future can be coalesced only
///   if all legs of the `Select*` future return the same type
///
/// - The outputs of the `embassy_futures::Join*` future can be coalesced only if
///   all legs of the `Join*` future return `Result<(), T>` where T is the same error type.
///   Note that in the case when multiple legs of the `Join*` future resulted in an error,
///   only the error of the leftmost leg is returned, while the others are discarded.
pub trait Coalesce<T> {
    fn coalesce(self) -> impl Future<Output = T>;
}

impl<T, F1, F2> Coalesce<T> for Select<F1, F2>
where
    F1: Future<Output = T>,
    F2: Future<Output = T>,
{
    async fn coalesce(self) -> T {
        match self.await {
            Either::First(t) => t,
            Either::Second(t) => t,
        }
    }
}

impl<T, F1, F2, F3> Coalesce<T> for Select3<F1, F2, F3>
where
    F1: Future<Output = T>,
    F2: Future<Output = T>,
    F3: Future<Output = T>,
{
    async fn coalesce(self) -> T {
        match self.await {
            Either3::First(t) => t,
            Either3::Second(t) => t,
            Either3::Third(t) => t,
        }
    }
}

impl<T, F1, F2, F3, F4> Coalesce<T> for Select4<F1, F2, F3, F4>
where
    F1: Future<Output = T>,
    F2: Future<Output = T>,
    F3: Future<Output = T>,
    F4: Future<Output = T>,
{
    async fn coalesce(self) -> T {
        match self.await {
            Either4::First(t) => t,
            Either4::Second(t) => t,
            Either4::Third(t) => t,
            Either4::Fourth(t) => t,
        }
    }
}

impl<T, F1, F2> Coalesce<Result<(), T>> for Join<F1, F2>
where
    F1: Future<Output = Result<(), T>>,
    F2: Future<Output = Result<(), T>>,
{
    async fn coalesce(self) -> Result<(), T> {
        match self.await {
            (Err(e), _) => Err(e),
            (_, Err(e)) => Err(e),
            _ => Ok(()),
        }
    }
}

impl<T, F1, F2, F3> Coalesce<Result<(), T>> for Join3<F1, F2, F3>
where
    F1: Future<Output = Result<(), T>>,
    F2: Future<Output = Result<(), T>>,
    F3: Future<Output = Result<(), T>>,
{
    async fn coalesce(self) -> Result<(), T> {
        match self.await {
            (Err(e), _, _) => Err(e),
            (_, Err(e), _) => Err(e),
            (_, _, Err(e)) => Err(e),
            _ => Ok(()),
        }
    }
}

impl<T, F1, F2, F3, F4> Coalesce<Result<(), T>> for Join4<F1, F2, F3, F4>
where
    F1: Future<Output = Result<(), T>>,
    F2: Future<Output = Result<(), T>>,
    F3: Future<Output = Result<(), T>>,
    F4: Future<Output = Result<(), T>>,
{
    async fn coalesce(self) -> Result<(), T> {
        match self.await {
            (Err(e), _, _, _) => Err(e),
            (_, Err(e), _, _) => Err(e),
            (_, _, Err(e), _) => Err(e),
            (_, _, _, Err(e)) => Err(e),
            _ => Ok(()),
        }
    }
}

impl<T, F1, F2, F3, F4, F5> Coalesce<Result<(), T>> for Join5<F1, F2, F3, F4, F5>
where
    F1: Future<Output = Result<(), T>>,
    F2: Future<Output = Result<(), T>>,
    F3: Future<Output = Result<(), T>>,
    F4: Future<Output = Result<(), T>>,
    F5: Future<Output = Result<(), T>>,
{
    async fn coalesce(self) -> Result<(), T> {
        match self.await {
            (Err(e), _, _, _, _) => Err(e),
            (_, Err(e), _, _, _) => Err(e),
            (_, _, Err(e), _, _) => Err(e),
            (_, _, _, Err(e), _) => Err(e),
            (_, _, _, _, Err(e)) => Err(e),
            _ => Ok(()),
        }
    }
}

/// Executes the provided task asynchronously by spawning it in a separate thread and
/// awaiting on the thread to signal its execution is complete
///
/// # Arguments
/// - `task_name` - The name of the task
/// - `task` - The task to execute
///
/// # Returns
/// The result of the task
pub async fn unblock<T, R>(task_name: &str, task: T) -> anyhow::Result<R>
where
    T: FnOnce() -> anyhow::Result<R> + Send + 'static,
    R: Send + 'static,
{
    let finished = Arc::new(Signal::<CriticalSectionRawMutex, ()>::new());

    let handle = {
        let finished = finished.clone();

        std::thread::Builder::new()
            .name(task_name.to_string())
            .spawn(move || {
                let result = task();

                finished.signal(());

                result
            })
            .with_context(|| format!("Spawning thread `{task_name}` failed"))?
    };

    let handle = RefCell::new(Some(handle));

    let _guard = scopeguard::guard(&handle, |handle| {
        if let Some(handle) = handle.borrow_mut().take() {
            match handle.join().unwrap() {
                Ok(_) => {}
                Err(err) => {
                    error!("Flashing returned an error: {err}");
                }
            }
        }
    });

    finished.wait().await;

    // There should be a thread handle as the guard had not kicked in yet at this point
    let handle = handle.borrow_mut().take().unwrap();

    handle.join().unwrap()
}

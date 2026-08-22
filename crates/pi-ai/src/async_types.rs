//! Executor-neutral boxed future and stream aliases from Architecture v2 part
//! 2 §9.2–§9.3.

use futures_core::Stream;
use std::{future::Future, pin::Pin};

/// A boxed future for single-threaded and local executors.
pub type LocalBoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// A boxed future that can move between executor threads.
pub type SendBoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A boxed stream for single-threaded and local executors.
pub type LocalBoxStream<'a, T> = Pin<Box<dyn Stream<Item = T> + 'a>>;

/// A boxed stream that can move between executor threads.
pub type SendBoxStream<'a, T> = Pin<Box<dyn Stream<Item = T> + Send + 'a>>;

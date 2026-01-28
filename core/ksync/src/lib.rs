//! [ArceOS](https://github.com/arceos-org/arceos) synchronization primitives.
//!
//! Currently supported primitives:
//!
//! - [`Mutex`]: A mutual exclusion primitive.
//! - mod [`spin`]: spinlocks imported from the [`kspin`] crate.
//!
//! # Cargo Features
//!
//! - `multitask`: For use in the multi-threaded environments. If the feature is
//!   not enabled, [`Mutex`] will be an alias of [`spin::SpinNoIrq`]. This
//!   feature is enabled by default.

#![cfg_attr(not(test), no_std)]
#![feature(doc_cfg)]

pub use kspin as spin;

mod mutex;

pub use self::mutex::{Mutex, MutexGuard, RawMutex};

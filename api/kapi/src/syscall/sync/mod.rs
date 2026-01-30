//! Synchronization and atomic operation syscalls.
//!
//! This module implements synchronization primitives and memory operations including:
//! - Futex operations (futex, futex2, etc.)
//! - Memory barriers (membarrier, etc.)
//! - Atomic memory operations

mod futex;
mod membarrier;

pub use self::{futex::*, membarrier::*};

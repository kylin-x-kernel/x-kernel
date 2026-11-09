// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! POSIX/Linux ABI carriers, checked user-memory copying, and time conversions.
//!
//! [`UserPtr`] and [`UserConstPtr`] copy values through `osvm`; constructing a
//! wrapper does not validate an address. [`UserRead`] and [`UserWrite`] describe
//! representation safety, not permissions or semantic validity. Syscall adapters
//! must validate flags, lengths, identities, and reserved fields after copying.
//!
//! [`IoVec`] and [`IoVectorBuf`] support scatter-gather marshalling;
//! [`TimeSpanLike`], [`SystemTimeLike`] and [`PosixClockTicks`] translate ABI time.
//! Public domain modules also attach copy traits to `linux_raw_sys` structures.
//! This crate owns temporary buffers and carriers, not processes or file objects.
//!
//! # Examples
//!
//! Pure carrier operations do not access user memory:
//!
//! ```
//! use posix_types::{FdSet, UserPtr, check_sigset_size};
//! let mut set = FdSet::zeroed();
//! set.set(3);
//! assert!(set.is_set(3));
//! assert!(UserPtr::<u32>::default().is_null());
//! assert!(check_sigset_size(8).is_ok());
//! ```

#![no_std]

/// A POSIX process identifier scalar.
///
/// X-Kernel currently also reuses this scalar carrier for session IDs, process
/// group IDs, and user-visible thread IDs. The semantic owner of those
/// identities lives in higher-level process-domain crates; this crate only
/// provides the shared ABI-sized integer carrier.
///
/// POSIX `pid_t` is a signed integer, but X-Kernel uses `u32` here only as the
/// internal storage carrier for non-negative identity numbers. Syscall
/// boundaries remain responsible for interpreting negative selector values such
/// as `waitpid(-1, ...)` or `kill(-pgid, ...)` before converting them into
/// process-domain lookups.
pub type Pid = u32;

/// A POSIX thread identifier scalar.
///
/// The semantic owner lives in the higher-level process-domain crate; this
/// crate only provides the shared ABI-sized integer carrier.
pub type Tid = u32;

pub mod fs;
pub mod io;
pub mod io_mpx;
pub mod ipc;
pub mod net;
pub mod process;
pub mod ptr;
pub mod quota;
pub mod signal;
pub mod sync;
pub mod system;
pub mod task;
pub mod time;

pub use io::*;
pub use io_mpx::*;
pub use ipc::*;
// Re-export derive macros for external users: `use posix_types::UserRead` in `#[derive(...)]`
pub use macros::{UserRead, UserWrite};
pub use ptr::*;
pub use signal::*;
pub use sync::*;
pub use time::*;

// Hidden module for macro-generated code. Avoids trait/derive-macro name collisions.
#[doc(hidden)]
pub mod __private {
    pub use crate::ptr::{UserRead as UserReadTrait, UserWrite as UserWriteTrait};
}

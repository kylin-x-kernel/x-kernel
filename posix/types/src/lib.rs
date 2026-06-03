// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! POSIX/Linux ABI types.

#![no_std]

pub mod fs;
pub mod io;
pub mod io_mpx;
pub mod ipc;
pub mod net;
pub mod process;
pub mod ptr;
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

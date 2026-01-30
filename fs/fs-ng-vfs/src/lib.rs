// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Next-generation VFS interfaces and data structures.
#![no_std]
#![allow(rustdoc::broken_intra_doc_links)]

#[cfg(feature = "unittest")]
#[macro_use]
extern crate log;

extern crate alloc;

mod fs;
mod mount;
mod node;
pub mod path;
mod types;

pub use fs::*;
pub use mount::*;
pub use node::*;
pub use types::*;

pub type VfsError = kerrno::KError;
pub type VfsResult<T> = Result<T, VfsError>;

use spin::{Mutex, MutexGuard};

#[cfg(feature = "unittest")]
pub mod test_path;
#[cfg(feature = "unittest")]
pub mod test_types;
#[cfg(feature = "unittest")]
pub mod test_unit_test;

#[cfg(feature = "unittest")]
pub use test_unit_test::fs_ng_vfs_unit_test;

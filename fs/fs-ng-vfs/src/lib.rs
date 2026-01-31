// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Next-generation VFS interfaces and data structures.
#![no_std]
#![allow(rustdoc::broken_intra_doc_links)]

extern crate alloc;

mod fs;
mod mount;
mod node;
pub mod path;
mod types;

mod test_path;
mod test_types;

pub use fs::*;
pub use mount::*;
pub use node::*;
pub use types::*;

pub type VfsError = kerrno::KError;
pub type VfsResult<T> = Result<T, VfsError>;

use spin::{Mutex, MutexGuard};

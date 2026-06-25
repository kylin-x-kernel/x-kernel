// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Next-generation VFS interfaces and data structures.
#![no_std]
#![allow(rustdoc::broken_intra_doc_links)]

extern crate alloc;

mod address_space;
mod file;
mod fs;
mod lookup;
mod mount;
mod namei;
mod node;
mod ops;
pub mod path;
mod permission;
mod types;

pub use address_space::*;
pub use file::*;
pub use fs::*;
pub use lookup::*;
pub use mount::*;
pub use namei::*;
pub use node::*;
pub use ops::*;
pub use permission::*;
pub use types::*;

pub type VfsError = kerrno::KError;
pub type VfsResult<T> = Result<T, VfsError>;

use ksync::{Mutex, MutexGuard};

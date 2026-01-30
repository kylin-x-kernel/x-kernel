// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Memory management syscalls.
//!
//! This module implements memory management operations including:
//! - Virtual memory mapping (mmap, munmap, mremap, etc.)
//! - Heap manipulation (brk, sbrk, etc.)
//! - Memory locking (mlock, mlockall, etc.)
//! - Memory synchronization (msync, etc.)
//! - Memory information queries (mincore, etc.)

mod brk;
mod mincore;
mod mmap;

pub use self::{brk::*, mincore::*, mmap::*};

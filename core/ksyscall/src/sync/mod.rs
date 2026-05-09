// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Synchronization and atomic operation syscalls.
//!
//! This module implements synchronization primitives and memory operations including:
//! - Memory barriers (membarrier, etc.)
//! - Atomic memory operations

mod membarrier;

pub use posix_sync::{sys_futex, sys_get_robust_list, sys_set_robust_list};

pub use self::membarrier::*;

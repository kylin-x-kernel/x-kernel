// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Synchronization and atomic operation syscalls.
//!
//! This module implements synchronization primitives and memory operations including:
//! - Futex operations (futex, futex2, etc.)
//! - Memory barriers (membarrier, etc.)
//! - Atomic memory operations

mod futex;
mod membarrier;

pub use self::{futex::*, membarrier::*};

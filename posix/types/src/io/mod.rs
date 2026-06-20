// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ABI-facing I/O structure definitions.

use linux_raw_sys::general::{epoll_event, pollfd};

use crate::ptr::{UserRead, UserWrite};

mod input;
mod iovec;
mod vector_buf;

pub use input::*;
pub use iovec::*;
pub use vector_buf::*;

// SAFETY: these event structs are POD syscall carriers whose bytes can be
// copied to and from user memory without additional invariants.
unsafe impl UserRead for epoll_event {}
// SAFETY: these event structs are POD syscall carriers whose bytes can be
// copied to and from user memory without additional invariants.
unsafe impl UserWrite for epoll_event {}
// SAFETY: these event structs are POD syscall carriers whose bytes can be
// copied to and from user memory without additional invariants.
unsafe impl UserRead for pollfd {}
// SAFETY: these event structs are POD syscall carriers whose bytes can be
// copied to and from user memory without additional invariants.
unsafe impl UserWrite for pollfd {}

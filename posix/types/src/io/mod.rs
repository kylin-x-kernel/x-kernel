// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ABI-facing I/O structure definitions.

use linux_raw_sys::general::{epoll_event, pollfd};

use crate::{UserRead, UserWrite};

mod input;
mod iovec;
mod vector_buf;

pub use input::*;
pub use iovec::*;
pub use vector_buf::*;

unsafe impl UserRead for epoll_event {}
unsafe impl UserWrite for epoll_event {}
unsafe impl UserRead for pollfd {}
unsafe impl UserWrite for pollfd {}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ABI-facing I/O structure definitions.

use core::mem::{align_of, offset_of, size_of};

use linux_raw_sys::general::{epoll_event, pollfd};

use crate::{UserRead, UserWrite};

mod input;
mod iovec;
mod vector_buf;

pub use input::*;
pub use iovec::*;
pub use vector_buf::*;

/// A `struct epoll_event` carrier used at the syscall boundary.
///
/// `linux_raw_sys::general::epoll_event` has four bytes of implicit padding
/// between `events` and `data` on every supported architecture except x86_64,
/// whose Linux ABI packs the structure. Copying that type to user memory would
/// expose uninitialized kernel bytes, so this carrier models the padding as an
/// explicit field that is always zero when the kernel builds an event.
#[cfg_attr(target_arch = "x86_64", repr(C, packed))]
#[cfg_attr(not(target_arch = "x86_64"), repr(C))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, UserRead, UserWrite)]
pub struct k_epoll_event {
    events: u32,
    #[cfg(not(target_arch = "x86_64"))]
    abi_pad: u32,
    data: u64,
}

impl k_epoll_event {
    /// Creates an event with the given readiness mask and user data.
    pub const fn new(events: u32, data: u64) -> Self {
        Self {
            events,
            #[cfg(not(target_arch = "x86_64"))]
            abi_pad: 0,
            data,
        }
    }

    /// Returns the readiness mask, such as `EPOLLIN | EPOLLET`.
    pub const fn events(self) -> u32 {
        self.events
    }

    /// Returns the user data registered for the file descriptor.
    pub const fn data(self) -> u64 {
        self.data
    }
}

const _: () = {
    assert!(size_of::<k_epoll_event>() == size_of::<epoll_event>());
    assert!(align_of::<k_epoll_event>() == align_of::<epoll_event>());
    assert!(offset_of!(k_epoll_event, events) == offset_of!(epoll_event, events));
    assert!(offset_of!(k_epoll_event, data) == offset_of!(epoll_event, data));
};

// SAFETY: `pollfd` is an `i32` followed by two `i16` fields, so it has no
// padding and its bytes can be copied to and from user memory.
unsafe impl UserRead for pollfd {}
// SAFETY: `pollfd` is an `i32` followed by two `i16` fields, so it has no
// padding and its bytes can be copied to and from user memory.
unsafe impl UserWrite for pollfd {}

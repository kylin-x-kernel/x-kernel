// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ABI-facing iovec carrier definitions.

extern crate alloc;

use alloc::vec::Vec;

use kerrno::{KError, KResult};

use crate::{UserConstPtr, UserRead};

/// An I/O vector descriptor used by `readv`/`writev`-style syscalls.
#[derive(Debug, Copy, Clone)]
#[repr(C)]
pub struct IoVec {
    pub iov_base: *mut u8,
    pub iov_len: isize,
}

impl IoVec {
    /// Loads a user-provided iovec array into kernel-owned descriptors.
    pub fn load_from_user(iovs: UserConstPtr<IoVec>, iovcnt: usize) -> KResult<Vec<IoVec>> {
        if iovcnt == 0 {
            return Ok(Vec::new());
        }

        iovs.check_non_null().ok_or(KError::BadAddress)?;
        iovs.load_vm_vec(iovcnt).map_err(Into::into)
    }
}

unsafe impl UserRead for IoVec {}

#[cfg(unittest)]
mod tests {
    use unittest::def_test;

    use super::*;

    #[def_test]
    fn test_iovec_layout() {
        assert_eq!(
            core::mem::size_of::<IoVec>(),
            core::mem::size_of::<*mut u8>() + core::mem::size_of::<isize>()
        );
    }
}

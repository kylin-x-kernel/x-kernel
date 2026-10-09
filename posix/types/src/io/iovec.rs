// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ABI-facing iovec carrier definitions.

extern crate alloc;

use alloc::vec::Vec;

use kerrno::{KError, KResult};

use crate::{UserConstPtr, UserRead};

/// An I/O vector descriptor used by `readv`/`writev`-style syscalls.
#[derive(Debug, Copy, Clone, UserRead)]
#[repr(C)]
pub struct IoVec {
    /// User virtual address of the segment; copying this descriptor does not validate it.
    pub iov_base: *mut u8,
    /// Segment length in bytes; adapters must reject negative lengths.
    pub iov_len: isize,
}

impl IoVec {
    /// Copies `iovcnt` descriptors into an owned vector in the current address space.
    ///
    /// A zero count returns an empty vector without touching `iovs`. This helper
    /// does not enforce `IOV_MAX` or validate segment lengths/addresses; callers
    /// must bound `iovcnt` before allocation and validate the copied descriptors.
    ///
    /// # Errors
    ///
    /// Returns `BadAddress` for a null pointer with nonzero count, or propagates
    /// the user-memory copy error through `KError`.
    ///
    /// # Panics
    ///
    /// An excessive vector capacity can panic; allocation failure follows the
    /// allocator's failure policy.
    pub fn load_from_user(iovs: UserConstPtr<IoVec>, iovcnt: usize) -> KResult<Vec<IoVec>> {
        if iovcnt == 0 {
            return Ok(Vec::new());
        }

        iovs.check_non_null().ok_or(KError::BadAddress)?;
        iovs.load_vm_vec(iovcnt).map_err(Into::into)
    }
}

#[unittest::mod_test]
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

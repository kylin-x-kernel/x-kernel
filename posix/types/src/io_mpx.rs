// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ABI-facing I/O multiplexing structures.

use core::fmt;

use linux_raw_sys::general::__FD_SETSIZE;

use crate::{UserConstPtr, UserPtr, UserRead, UserWrite, k_sigset};

/// An `fd_set`-compatible bitset used by `select`-style syscalls.
#[repr(C)]
#[derive(Clone, Copy, UserRead, UserWrite)]
pub struct FdSet {
    fds_bits: [usize; Self::FD_SETSIZE / usize::BITS as usize],
}

impl FdSet {
    pub const FD_SETSIZE: usize = __FD_SETSIZE as usize;

    pub fn zeroed() -> Self {
        Self {
            fds_bits: [0; Self::FD_SETSIZE / usize::BITS as usize],
        }
    }

    pub fn set(&mut self, fd: usize) {
        debug_assert!(fd < Self::FD_SETSIZE);
        self.fds_bits[fd / usize::BITS as usize] |= 1 << (fd % usize::BITS as usize);
    }

    pub fn is_set(&self, fd: usize) -> bool {
        debug_assert!(fd < Self::FD_SETSIZE);
        (self.fds_bits[fd / usize::BITS as usize] & (1 << (fd % usize::BITS as usize))) != 0
    }

    pub fn clear(&mut self) {
        self.fds_bits.fill(0);
    }

    /// Reads an [`FdSet`] from user space and masks bits above `nfds`.
    pub fn read_from_user(ptr: UserPtr<Self>, nfds: usize) -> osvm::MemResult<Option<Self>> {
        if ptr.is_null() {
            return Ok(None);
        }

        let mut fdset = ptr.read_vm()?;
        let full_words = nfds / usize::BITS as usize;
        let remaining = nfds % usize::BITS as usize;
        if remaining > 0 && full_words < fdset.fds_bits.len() {
            fdset.fds_bits[full_words] &= (1usize << remaining) - 1;
        }
        for word in fdset
            .fds_bits
            .iter_mut()
            .skip(full_words + usize::from(remaining > 0))
        {
            *word = 0;
        }

        Ok(Some(fdset))
    }

    /// Writes this [`FdSet`] back to user space unless `ptr` is null.
    pub fn write_to_user(&self, ptr: UserPtr<Self>) -> osvm::MemResult {
        if ptr.is_null() {
            return Ok(());
        }

        ptr.write_vm(*self)
    }
}

impl fmt::Debug for FdSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries((0..Self::FD_SETSIZE).filter(|&fd| self.is_set(fd)))
            .finish()
    }
}

/// The raw `pselect6` signal-mask argument wrapper.
#[repr(C)]
#[derive(Clone, Copy, UserRead)]
pub struct SignalSetWithSize {
    set: UserConstPtr<k_sigset>,
    sigsetsize: usize,
}

impl SignalSetWithSize {
    pub fn set(self) -> UserConstPtr<k_sigset> {
        self.set
    }

    pub fn sigsetsize(self) -> usize {
        self.sigsetsize
    }
}

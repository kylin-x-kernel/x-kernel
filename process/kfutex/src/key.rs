// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::mem::size_of;

use kerrno::{KError, KResult};
use kuaccess::check_access;
use memspace::{FutexBacking, MmSpace, VmObjectId};

/// Stable identity of one Linux futex word.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FutexKey(FutexKeyKind);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum FutexKeyKind {
    Private {
        mm_id: u64,
        address: usize,
    },
    Shared {
        object: VmObjectId,
        page_index: u64,
        offset_in_page: u16,
    },
}

impl FutexKey {
    /// Collision-free ordering projection for the per-bucket PI index.
    pub(crate) fn pi_index(self) -> (u8, u64, u64, usize) {
        match self.0 {
            FutexKeyKind::Private { mm_id, address } => (0, mm_id, 0, address),
            FutexKeyKind::Shared {
                object,
                page_index,
                offset_in_page,
            } => {
                let (kind, id) = match object {
                    VmObjectId::File(id) => (1, id.raw()),
                    VmObjectId::Anon(id) => (2, id.raw()),
                };
                (kind, id, page_index, usize::from(offset_in_page))
            }
        }
    }

    /// Resolves a private futex key without consulting VMA metadata.
    ///
    /// Used for `FUTEX_PRIVATE_FLAG`: only alignment / user-range checks are
    /// required, so callers can avoid taking the address-space lock.
    ///
    /// # Errors
    ///
    /// Returns `EINVAL` (`InvalidInput`) when `address` is not `u32`-aligned,
    /// or `EFAULT` when the word is outside the accessible user range.
    pub fn resolve_private(mm_id: u64, address: usize) -> KResult<Self> {
        if !address.is_multiple_of(size_of::<u32>()) {
            return Err(KError::InvalidInput);
        }
        check_access(address, size_of::<u32>()).map_err(KError::from)?;
        Ok(Self(FutexKeyKind::Private { mm_id, address }))
    }

    /// Resolves a key for a syscall operation.
    ///
    /// `is_private` implements `FUTEX_PRIVATE_FLAG`: it deliberately bypasses
    /// VMA lookup after validating the user range. Non-private operations use
    /// MM-owned backing metadata to distinguish private and shared mappings.
    ///
    /// Prefer [`Self::resolve_private`] on the private path when the caller
    /// already has `mm_id` and does not hold the address-space lock.
    ///
    /// # Errors
    ///
    /// Returns `EINVAL` when `address` is not `u32`-aligned, `EFAULT` when
    /// the word is outside the accessible user range, or the error reported
    /// by `MmSpace::resolve_futex_backing` for unmapped addresses.
    pub fn resolve(aspace: &MmSpace, address: usize, is_private: bool) -> KResult<Self> {
        if is_private {
            return Self::resolve_private(aspace.mm_id(), address);
        }
        if !address.is_multiple_of(size_of::<u32>()) {
            return Err(KError::InvalidInput);
        }
        check_access(address, size_of::<u32>()).map_err(KError::from)?;

        Ok(Self(match aspace.resolve_futex_backing(address)? {
            FutexBacking::Private { mm_id, address } => FutexKeyKind::Private { mm_id, address },
            FutexBacking::Shared {
                object,
                page_index,
                offset_in_page,
            } => FutexKeyKind::Shared {
                object,
                page_index,
                offset_in_page,
            },
        }))
    }

    #[cfg(unittest)]
    pub(crate) fn private_for_test(mm_id: u64, address: usize) -> Self {
        Self(FutexKeyKind::Private { mm_id, address })
    }
}

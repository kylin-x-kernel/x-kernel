// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::mem::size_of;

use kerrno::{KError, KResult};
use kuaccess::check_access;
use memspace::{FutexBacking, MmSpace, VmObjectId};

/// Stable identity of one Linux futex word.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FutexKey(FutexKeyKind);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
    /// Resolves a private futex key without consulting VMA metadata.
    ///
    /// Used for `FUTEX_PRIVATE_FLAG`: only alignment / user-range checks are
    /// required, so callers can avoid taking the address-space lock.
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

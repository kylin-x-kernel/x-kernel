// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use memaddr::VirtAddr;
use memspace::{MmSpace, VmBackingKind, VmObjectId};

/// A key that uniquely identifies a futex in the system.
pub enum FutexKey {
    /// A futex that is private to the current process.
    Private {
        /// The memory address of the futex.
        address: usize,
    },

    /// A futex in a shared memory region.
    Shared {
        /// The offset of the futex within the shared memory region.
        offset: usize,
        /// The shared memory region.
        region: SharedRegionIdentity,
    },
}

/// Identity of a shared futex backing object.
pub enum SharedRegionIdentity {
    /// Shared anonymous memory object identity.
    Anonymous(VmObjectId),
    /// File-backed shared object identity.
    File(VmObjectId),
}

impl FutexKey {
    /// Creates a new `FutexKey`.
    pub fn new(aspace: &MmSpace, address: usize) -> Self {
        let vaddr = VirtAddr::from_usize(address);
        if let Some(vma) = aspace.find_vma(vaddr) {
            match vma.backing().kind() {
                VmBackingKind::AnonymousShared { object } => {
                    return Self::Shared {
                        offset: address - vma.start().as_usize(),
                        region: SharedRegionIdentity::Anonymous(object),
                    };
                }
                VmBackingKind::FileShared { object } => {
                    return Self::Shared {
                        offset: address - vma.start().as_usize(),
                        region: SharedRegionIdentity::File(object),
                    };
                }
                _ => {}
            }
        }
        Self::Private { address }
    }

    pub(crate) fn as_usize(&self) -> usize {
        match self {
            FutexKey::Private { address } => *address,
            FutexKey::Shared { offset, .. } => *offset,
        }
    }
}

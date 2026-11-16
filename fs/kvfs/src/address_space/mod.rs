// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Inode-owned address space and filesystem operation boundary.

use alloc::{collections::BTreeMap, sync::Arc};
use core::sync::atomic::{AtomicU64, Ordering};

use pagecache::{CacheResidency, MappingPages};
use views::RegisteredView;
use vmobj::{FileObjectId, MappingViewId, VmObjectId};

use crate::{Mutex, RwLock, VfsError, VfsResult, WeakVfsInode};

mod filemap;
mod operations;
mod readahead;
mod truncate;
mod views;
mod writeback;
mod writeback_errors;

pub use operations::{
    AddressSpaceOperations, BatchWriteRunTail, BatchedWriteOperations, WriteBeginRequest,
    WriteEndRequest, WriteReservation,
};
pub(crate) use operations::{default_address_space_operations, empty_address_space_operations};
pub use pagecache::{WritebackControl, WritebackSyncMode};
pub use readahead::ReadaheadControl;
pub use views::AddressSpaceViewGuard;
pub use writeback_errors::WritebackErrors;

static NEXT_ADDRESS_SPACE_ID: AtomicU64 = AtomicU64::new(1);

/// VFS address space for one inode.
pub struct AddressSpace {
    inode: WeakVfsInode,
    /// Shared operation table corresponding to Linux `address_space::a_ops`.
    ops: &'static dyn AddressSpaceOperations,
    /// Batched-write tail-release vtable, sampled once from `ops`; `None` keeps
    /// every buffered write on the per-folio path.
    batched: Option<&'static dyn BatchedWriteOperations>,
    object_id: VmObjectId,
    invalidate_lock: RwLock<()>,
    views: Mutex<BTreeMap<MappingViewId, RegisteredView>>,
    /// Resident folios and dirty marks, corresponding to Linux `i_pages`.
    i_pages: MappingPages,
    writeback_errors: WritebackErrors,
}

impl AddressSpace {
    /// Creates an address space for `inode`.
    pub fn new(inode: WeakVfsInode, ops: &'static dyn AddressSpaceOperations) -> Arc<Self> {
        Self::with_residency(inode, ops, None)
    }

    pub(crate) fn with_residency(
        inode: WeakVfsInode,
        ops: &'static dyn AddressSpaceOperations,
        residency: Option<Arc<dyn CacheResidency>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            inode,
            ops,
            batched: ops.batched_write_ops(),
            object_id: VmObjectId::File(FileObjectId::from_raw(
                NEXT_ADDRESS_SPACE_ID.fetch_add(1, Ordering::Relaxed),
            )),
            invalidate_lock: RwLock::new(()),
            views: Mutex::new(BTreeMap::new()),
            i_pages: MappingPages::with_residency(residency),
            writeback_errors: WritebackErrors::new(),
        })
    }

    /// Returns the inode owning this address space.
    pub fn inode(&self) -> Option<alloc::sync::Arc<crate::VfsInode>> {
        self.inode.upgrade()
    }

    /// Returns the stable VM identity of this inode address space.
    pub fn object_id(&self) -> VmObjectId {
        self.object_id
    }

    /// Returns `address_space::nrpages`.
    pub fn nrpages(&self) -> u64 {
        self.i_pages.nrpages()
    }

    /// Borrows the error channel embedded in this address space.
    pub fn writeback_errors(&self) -> &WritebackErrors {
        &self.writeback_errors
    }

    fn visible_len(&self) -> VfsResult<u64> {
        self.inode()
            .map(|inode| inode.size())
            .ok_or(VfsError::InvalidInput)
    }
}

impl core::fmt::Debug for AddressSpace {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AddressSpace")
            .field("inode", &self.inode().map(|inode| inode.inode()))
            .field("object_id", &self.object_id())
            .field("nrpages", &self.nrpages())
            .finish()
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/address_space/mod.rs");

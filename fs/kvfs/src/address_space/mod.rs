// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Inode-owned address space and filesystem operation boundary.

use alloc::{collections::BTreeMap, sync::Arc};
use core::sync::atomic::{AtomicU64, Ordering};

use pagecache::{PageCache, PageCacheKind};
use views::RegisteredView;
use vmobj::{FileObjectId, MappingViewId, VmObjectId};

use crate::{Mutex, NodeFlags, RwLock, VfsError, VfsResult, WeakVfsInode};

mod filemap;
mod operations;
mod readahead;
mod truncate;
mod views;
mod writeback;

pub use operations::{
    AddressSpaceOperations, PageMkwriteRequest, WriteBeginRequest, WriteEndRequest,
};
pub(crate) use operations::{default_address_space_operations, empty_address_space_operations};
pub use readahead::ReadaheadControl;
pub use views::AddressSpaceViewGuard;
pub use writeback::{WritebackControl, WritebackSyncMode};

static NEXT_ADDRESS_SPACE_ID: AtomicU64 = AtomicU64::new(1);

/// VFS address space for one inode.
pub struct AddressSpace {
    inode: WeakVfsInode,
    /// Shared operation table corresponding to Linux `address_space::a_ops`.
    ops: &'static dyn AddressSpaceOperations,
    object_id: VmObjectId,
    invalidate_lock: RwLock<()>,
    views: Mutex<BTreeMap<MappingViewId, RegisteredView>>,
    page_cache: Arc<PageCache>,
}

impl AddressSpace {
    /// Creates an address space for `inode`.
    pub fn new(
        inode: WeakVfsInode,
        ops: &'static dyn AddressSpaceOperations,
        kind: PageCacheKind,
    ) -> Arc<Self> {
        Arc::new(Self {
            inode,
            ops,
            object_id: VmObjectId::File(FileObjectId::from_raw(
                NEXT_ADDRESS_SPACE_ID.fetch_add(1, Ordering::Relaxed),
            )),
            invalidate_lock: RwLock::new(()),
            views: Mutex::new(BTreeMap::new()),
            page_cache: PageCache::new(kind),
        })
    }

    /// Creates the default address space for an inode with the given flags.
    pub fn new_default(
        inode: WeakVfsInode,
        ops: &'static dyn AddressSpaceOperations,
        inode_flags: NodeFlags,
    ) -> Arc<Self> {
        Self::new(inode, ops, address_space_mapping_kind(inode_flags))
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
        self.page_cache.nrpages()
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

pub(crate) fn address_space_mapping_kind(inode_flags: NodeFlags) -> PageCacheKind {
    if inode_flags.contains(NodeFlags::ALWAYS_CACHE) {
        PageCacheKind::InMemory
    } else {
        PageCacheKind::FileBacked
    }
}

#[cfg(unittest)]
mod tests;

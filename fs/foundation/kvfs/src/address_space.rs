// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Inode address-space objects.
//!
//! Linux hangs the page-cache and backing-store operation boundary off
//! `inode->i_mapping`. This module provides the VFS-owned object that will
//! become that attachment point as filesystems migrate away from byte-level
//! `FileNodeOps` I/O.

use alloc::sync::Arc;

use crate::{AddressSpaceOperations, Mutex, MutexGuard, TypeMap, VfsResult, WeakVfsInode};

/// VFS address space for one inode.
pub struct AddressSpace {
    inode: WeakVfsInode,
    ops: Arc<dyn AddressSpaceOperations>,
    data: Mutex<TypeMap>,
}

impl AddressSpace {
    /// Creates an address space for `inode`.
    pub fn new(inode: WeakVfsInode, ops: Arc<dyn AddressSpaceOperations>) -> Self {
        Self {
            inode,
            ops,
            data: Mutex::default(),
        }
    }

    /// Returns the inode owning this address space.
    pub fn inode(&self) -> Option<alloc::sync::Arc<crate::VfsInode>> {
        self.inode.upgrade()
    }

    /// Returns the backing address-space operations.
    pub fn operations(&self) -> &Arc<dyn AddressSpaceOperations> {
        &self.ops
    }

    /// Reads one page from backing storage.
    pub fn read_page(&self, page_index: u64, page: &mut [u8]) -> VfsResult<usize> {
        self.ops.read_page(page_index, page)
    }

    /// Writes one page to backing storage.
    pub fn write_page(&self, page_index: u64, page: &[u8]) -> VfsResult<usize> {
        self.ops.write_page(page_index, page)
    }

    /// Writes dirty pages belonging to this address space.
    pub fn writepages(&self, data_only: bool) -> VfsResult<()> {
        self.ops.writepages(data_only)
    }

    /// Invalidates cached pages starting at `page_index`.
    pub fn invalidate_from(&self, page_index: u64) -> VfsResult<()> {
        self.ops.invalidate_from(page_index)
    }

    /// Access address-space-private attachment storage.
    pub fn data(&self) -> MutexGuard<'_, TypeMap> {
        self.data.lock()
    }
}

impl core::fmt::Debug for AddressSpace {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AddressSpace")
            .field("inode", &self.inode().map(|inode| inode.inode()))
            .finish()
    }
}

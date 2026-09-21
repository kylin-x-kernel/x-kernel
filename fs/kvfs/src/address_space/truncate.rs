// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Inode size publication and cache teardown.

use memaddr::PAGE_SIZE_4K;

use super::AddressSpace;
use crate::{VfsError, VfsResult};

impl AddressSpace {
    /// Changes the backing file length through this address space.
    pub fn set_len(&self, len: u64) -> VfsResult<()> {
        let _invalidate_guard = self.invalidate_lock.write();
        self.ops.set_len(self, len)
    }

    /// Publishes `inode::i_size` and updates mapped/cache state in Linux order.
    ///
    /// Filesystem `set_len` implementations call this after preparing their
    /// backing-store mutation and before releasing blocks. The enclosing
    /// [`Self::set_len`] call holds the address-space invalidate lock
    /// exclusively. Shrink performs `i_size_write`, unmap, cache truncation,
    /// then a second unmap so a concurrent private COW fault cannot survive
    /// beyond the new EOF.
    pub fn truncate_setsize(&self, len: u64) -> VfsResult<()> {
        let inode = self.inode().ok_or(VfsError::InvalidInput)?;
        let old_len = inode.size();
        inode.set_size(len);
        self.truncate_pagecache(old_len, len);
        Ok(())
    }

    /// Publishes a backing-store size change made outside the split set-length path.
    pub(crate) fn truncate_setsize_after_backing_change(&self, len: u64) -> VfsResult<()> {
        let _invalidate_guard = self.invalidate_lock.write();
        self.truncate_setsize(len)
    }

    /// Publishes the size accepted by `AddressSpaceOperations::write_end`.
    ///
    /// The caller must be the generic write path, which holds the inode data
    /// lock and has already prepared any cache bytes between the old EOF and
    /// the completed write. This is the Rust equivalent of an aops
    /// `write_end` implementation doing `i_size_write(mapping->host, len)`.
    pub fn write_end_set_size(&self, len: u64) -> VfsResult<()> {
        let inode = self.inode().ok_or(VfsError::InvalidInput)?;
        if len > inode.size() {
            inode.set_size(len);
        }
        Ok(())
    }

    fn truncate_pagecache(&self, old_len: u64, len: u64) {
        let invalid_start = (len < old_len).then(|| {
            len.div_ceil(PAGE_SIZE_4K as u64)
                .saturating_mul(PAGE_SIZE_4K as u64)
        });
        if let Some(start) = invalid_start {
            self.invalidate_mappings_from(start);
        }
        self.i_pages.resize_cached_folios(old_len, len);
        if let Some(start) = invalid_start {
            self.invalidate_mappings_from(start);
        }
    }

    /// Drops cached pages during final inode teardown.
    ///
    /// Ordinary invalidation still requires dirty data to be written back
    /// first, while final inode teardown may discard dirty cached pages because
    /// the inode is leaving the VFS lifecycle.
    pub fn evict(&self) -> VfsResult<()> {
        self.truncate_final()
    }

    /// Drops cached pages during final inode teardown.
    pub fn truncate_final(&self) -> VfsResult<()> {
        let _invalidate_guard = self.invalidate_lock.write();
        self.i_pages.truncate_final()?;
        Ok(())
    }
}

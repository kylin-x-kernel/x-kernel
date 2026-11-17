// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Inode size publication and cache teardown.

use memaddr::PAGE_SIZE_4K;

use super::AddressSpace;
use crate::{RwLockWriteGuard, VfsError, VfsResult};

impl AddressSpace {
    pub(crate) fn lock_invalidate_exclusive(&self) -> RwLockWriteGuard<'_, ()> {
        self.invalidate_lock.write()
    }

    /// Publishes `inode::i_size` and updates mapped/cache state in Linux order.
    ///
    /// Filesystem inode `set_len` implementations call this after preparing their
    /// backing-store mutation and before releasing blocks. The inode truncate
    /// path holds the address-space invalidate lock exclusively. Shrink performs
    /// `i_size_write`, unmap, cache truncation,
    /// then a second unmap so a concurrent private COW fault cannot survive
    /// beyond the new EOF.
    pub(crate) fn truncate_setsize(&self, len: u64) -> VfsResult<()> {
        if !ktask::is_sleepable_context() {
            return Err(VfsError::InvalidInput);
        }
        let inode = self.inode().ok_or(VfsError::InvalidInput)?;
        let old_len = inode.size();
        inode.set_size(len);
        self.truncate_pagecache(old_len, len)?;
        Ok(())
    }

    /// Publishes a backing-store size change made outside the split set-length path.
    pub(crate) fn truncate_setsize_after_backing_change(&self, len: u64) -> VfsResult<()> {
        let _invalidate_guard = self.invalidate_lock.write();
        self.truncate_setsize(len)
    }

    /// Publishes the size accepted by `AddressSpaceOperations::write_end`.
    ///
    /// The caller must be the generic per-folio write path, which holds the
    /// inode data lock and the copied folio lock. Batched runs release their
    /// unaccepted tail through `BatchedWriteOperations::release_write_run_tail`
    /// inside the visible size and never publish a size here. The path has prepared any cache bytes
    /// between the old EOF and the completed write. This is the Rust
    /// equivalent of an aops `write_end` implementation doing
    /// `i_size_write(mapping->host, len)`.
    ///
    /// `len` is the nonempty accepted prefix's end in bytes. This helper reads
    /// the owning inode's current size under the caller's inode data lock and
    /// publishes `len` only for growth.
    ///
    /// # Errors
    /// Returns `InvalidInput` if the owning inode cannot be resolved.
    pub fn write_end_set_size(&self, len: u64) -> VfsResult<()> {
        let inode = self.inode().ok_or(VfsError::InvalidInput)?;
        if len > inode.size() {
            inode.set_size(len);
        }
        Ok(())
    }

    fn truncate_pagecache(&self, old_len: u64, len: u64) -> VfsResult<()> {
        let invalid_start = (len < old_len).then(|| {
            len.div_ceil(PAGE_SIZE_4K as u64)
                .saturating_mul(PAGE_SIZE_4K as u64)
        });
        if let Some(start) = invalid_start {
            self.invalidate_mappings_from(start);
        }
        self.i_pages.resize_cached_folios(old_len, len)?;
        if let Some(start) = invalid_start {
            self.invalidate_mappings_from(start);
        }
        Ok(())
    }

    /// Unmaps every view covering `[start, end)` and discards the clean cached
    /// folios in the same range.
    ///
    /// - `start`: inclusive byte offset.
    /// - `end`: exclusive byte offset; an empty or inverted range is a no-op.
    ///
    /// Returns: `true` when the range held no cached folio or when every
    /// covering view completed invalidation and every folio was discarded;
    /// `false` when a target address space was busy, in which case nothing is
    /// discarded.
    ///
    /// Errors: `ResourceBusy` when a folio stayed dirty after the round; clean
    /// folios are discarded before that error is returned. `OperationNotSupported`
    /// when a covering view has no notifier. `InvalidInput` outside sleepable
    /// task context.
    ///
    /// Locking: holds the address-space invalidate lock exclusively across unmap
    /// and discard. The caller must hold the inode data lock so a buffered write
    /// cannot repopulate a range this call just discarded; it must not hold an
    /// address-space lock nor a lock required by writeback completion. May
    /// sleep waiting for writeback.
    pub fn invalidate_range(&self, start: u64, end: u64) -> VfsResult<bool> {
        if start >= end {
            return Ok(true);
        }
        if !ktask::is_sleepable_context() {
            return Err(VfsError::InvalidInput);
        }
        let _invalidate_guard = self.invalidate_lock.write();
        if !self.try_invalidate_mappings_range(start, Some(end))? {
            return Ok(false);
        }
        self.i_pages.invalidate_pages_range(start, end)?;
        Ok(true)
    }

    /// Flushes and discards cached pages of a live inode after revoking mappings.
    ///
    /// The caller excludes buffered writers and backing-store policy changes.
    /// Faults are excluded by the invalidate lock. Every registered mapping must
    /// finish PTE removal and TLB invalidation before writeback and cache removal.
    /// A busy MM is retried without the invalidate lock, so its fault can unwind.
    /// Callers must not hold an MM lock or a lock needed by writeback completion.
    /// The writeback op runs directly, not through the public writepages wrapper,
    /// so it does not re-enter the invalidate read lock under the write lock.
    /// Errors leave cache ownership intact, although mappings may be revoked.
    pub fn invalidate_cached_pages(&self) -> VfsResult<()> {
        if !ktask::is_sleepable_context() {
            return Err(VfsError::InvalidInput);
        }
        loop {
            let guard = self.invalidate_lock.write();
            if !self.try_invalidate_mappings_range(0, None)? {
                drop(guard);
                ktask::yield_now();
                continue;
            }
            // Drain without reentering the invalidate lock, and retain failed data.
            self.write_and_wait_range_with(0..u64::MAX, |control| {
                self.ops.writepages(self, control)
            })?;
            self.i_pages.truncate_final()?;
            return Ok(());
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

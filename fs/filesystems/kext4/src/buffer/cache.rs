// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{collections::BTreeMap, sync::Arc, vec};
use core::{
    ops::Bound::{Excluded, Included, Unbounded},
    sync::atomic::{AtomicBool, Ordering},
};

#[cfg(unittest)]
use super::slot::MetadataBufferState;
use super::{
    access::{MetadataBuffer, MetadataWriteAccess, MetadataWriteback},
    slot::MetadataBufferSlot,
    sync::{Mutex, lock},
};
use crate::{
    Ext4Result, FilesystemBlock,
    io::FilesystemDevice,
    jbd2::{JournalCommitBlock, TransactionId},
};

unittest::path_mod_test!(tests, "../../ut-tests/buffer/cache/tests.rs");

struct MetadataBlockCacheInner {
    device: Arc<FilesystemDevice>,
    entries: Mutex<MetadataCacheIndex>,
    /// Advisory only: the index mutex still protects all eviction decisions.
    has_reclaim_demand: AtomicBool,
}

/// Soft retention allowance for resident metadata block payloads.
///
/// Transaction-owned and pinned blocks may exceed this allowance. Frozen image
/// storage and index overhead are not included, so this is not a memory limit.
pub(super) const METADATA_CACHE_RETAINED_BYTES: usize = 4 * 1024 * 1024;

struct MetadataCacheIndex {
    blocks: BTreeMap<FilesystemBlock, MetadataCacheEntry>,
    reclaim_cursor: Option<ReclaimCursor>,
}

struct MetadataCacheEntry {
    slot: Arc<MetadataBufferSlot>,
    /// Reuse since insertion or the previous scan, protected by the index lock.
    was_referenced: bool,
}

impl MetadataCacheEntry {
    fn new(slot: Arc<MetadataBufferSlot>) -> Self {
        Self {
            slot,
            // Loading a block once does not make a streaming read hot. A hit
            // earns a second chance without an extra lock or atomic update.
            was_referenced: false,
        }
    }

    fn referenced_slot(&mut self) -> Arc<MetadataBufferSlot> {
        self.was_referenced = true;
        self.slot.clone()
    }

    fn is_reclaimable(&self) -> bool {
        Arc::strong_count(&self.slot) == 1 && self.slot.is_reclaimable()
    }
}

#[derive(Clone, Copy)]
struct ReclaimCursor {
    after: FilesystemBlock,
    through: FilesystemBlock,
}

/// Per-filesystem cache of immutable ext4 metadata blocks.
///
/// The index deliberately owns ready slots. This is an explicit retain and
/// reclaim cache: handles pin the immutable byte storage, while
/// `reclaim_unused` removes entries whose bytes have no external readers.
///
/// Ordering contract (required by the allocator's prefetch pattern):
/// every successful publish through a write access replaces the bytes held
/// by the slot, and every read returns slot contents, loading the device
/// first only when the slot is missing or unloaded. There is no
/// write-through mode that bypasses the slot update. The allocator warms
/// the cache before taking its lock (prefetch) and re-reads inside the
/// critical section; that pattern is only race-free while this contract
/// holds, so a future write-through downgrade must be rejected.
#[derive(Clone)]
pub(super) struct MetadataBlockCache {
    inner: Arc<MetadataBlockCacheInner>,
}

impl MetadataBlockCache {
    pub(super) fn new(device: Arc<FilesystemDevice>) -> Self {
        Self {
            inner: Arc::new(MetadataBlockCacheInner {
                device,
                entries: Mutex::new(MetadataCacheIndex {
                    blocks: BTreeMap::new(),
                    reclaim_cursor: None,
                }),
                has_reclaim_demand: AtomicBool::new(false),
            }),
        }
    }

    fn load_slot(&self, block: FilesystemBlock) -> Ext4Result<Arc<MetadataBufferSlot>> {
        let (slot, is_loader) = {
            let mut entries = lock(&self.inner.entries);
            match entries.blocks.get_mut(&block) {
                Some(entry) => (entry.referenced_slot(), false),
                None => {
                    let slot = Arc::new(MetadataBufferSlot::new());
                    entries
                        .blocks
                        .insert(block, MetadataCacheEntry::new(slot.clone()));
                    self.update_reclaim_demand(&entries);
                    (slot, true)
                }
            }
        };

        if is_loader {
            let mut bytes = vec![0; self.inner.device.block_size()];
            if let Err(error) = self.inner.device.read_blocks(block, 1, &mut bytes) {
                slot.fail(error);
                let mut entries = lock(&self.inner.entries);
                if entries
                    .blocks
                    .get(&block)
                    .is_some_and(|cached| Arc::ptr_eq(&cached.slot, &slot))
                {
                    entries.blocks.remove(&block);
                    self.update_reclaim_demand(&entries);
                }
                return Err(error);
            }
            slot.publish(Arc::from(bytes.into_boxed_slice()));
        } else {
            slot.wait_until_loaded();
        }

        slot.load_result()?;
        Ok(slot)
    }

    pub(super) fn read(&self, block: FilesystemBlock) -> Ext4Result<MetadataBuffer> {
        self.load_slot(block)?.result()
    }

    #[allow(dead_code)]
    pub(super) fn write_access(
        &self,
        block: FilesystemBlock,
        transaction: TransactionId,
    ) -> Ext4Result<MetadataWriteAccess> {
        self.load_slot(block)?.begin_write_access(transaction)
    }

    #[allow(dead_code)]
    pub(super) fn create_access(
        &self,
        block: FilesystemBlock,
        transaction: TransactionId,
    ) -> Ext4Result<MetadataWriteAccess> {
        let zeroed_bytes: Arc<[u8]> =
            Arc::from(vec![0; self.inner.device.block_size()].into_boxed_slice());
        let (slot, needs_wait) = {
            let mut entries = lock(&self.inner.entries);
            match entries.blocks.get_mut(&block) {
                Some(entry) => (entry.referenced_slot(), true),
                None => {
                    let slot = Arc::new(MetadataBufferSlot::new());
                    entries
                        .blocks
                        .insert(block, MetadataCacheEntry::new(slot.clone()));
                    self.update_reclaim_demand(&entries);
                    slot.publish(zeroed_bytes.clone());
                    (slot, false)
                }
            }
        };

        if needs_wait {
            slot.wait_until_loaded();
            slot.load_result()?;
        }
        slot.begin_create_access(transaction, zeroed_bytes)
    }

    pub(super) fn forget_or_revoke_checkpointed(
        &self,
        block: FilesystemBlock,
        transaction: TransactionId,
    ) -> Ext4Result<()> {
        let slot = {
            let entries = lock(&self.inner.entries);
            entries.blocks.get(&block).map(|entry| entry.slot.clone())
        };
        let Some(slot) = slot else {
            return Ok(());
        };

        slot.wait_until_loaded();
        slot.load_result()?;
        slot.forget_or_revoke_checkpointed(transaction)?;

        if slot.is_reclaimable() {
            let mut entries = lock(&self.inner.entries);
            if entries
                .blocks
                .get(&block)
                .is_some_and(|cached| Arc::ptr_eq(&cached.slot, &slot))
            {
                entries.blocks.remove(&block);
                self.update_reclaim_demand(&entries);
            }
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub(super) fn start_writeback(
        &self,
        block: FilesystemBlock,
        transaction: TransactionId,
    ) -> Ext4Result<MetadataWriteback> {
        self.load_slot(block)?.begin_writeback(transaction)
    }

    pub(super) fn checkpoint_block(
        &self,
        block: FilesystemBlock,
        transaction: TransactionId,
    ) -> Ext4Result<()> {
        let Some(writeback) = self.load_slot(block)?.begin_checkpoint(transaction)? else {
            return Ok(());
        };
        let snapshot = writeback.snapshot();
        if let Err(error) = self
            .inner
            .device
            .write_contiguous_blocks(block, 1, snapshot.as_ref())
        {
            writeback.fail(error);
            return Err(error);
        }
        writeback.finish_checkpoint()
    }

    #[cfg(unittest)]
    pub(super) fn begin_checkpoint_for_test(
        &self,
        block: FilesystemBlock,
        transaction: TransactionId,
    ) -> Ext4Result<Option<MetadataWriteback>> {
        self.load_slot(block)?.begin_checkpoint(transaction)
    }

    pub(super) fn journal_commit_block(
        &self,
        block: FilesystemBlock,
        transaction: TransactionId,
    ) -> Ext4Result<JournalCommitBlock> {
        let snapshot = self.load_slot(block)?.snapshot_for_commit(transaction)?;
        Ok(JournalCommitBlock::new(block, snapshot.into_bytes()))
    }

    pub(super) fn replace_checkpoint_bytes(
        &self,
        block: FilesystemBlock,
        transaction: TransactionId,
        bytes: Arc<[u8]>,
    ) -> Ext4Result<()> {
        self.load_slot(block)?
            .replace_checkpoint_bytes(transaction, bytes)
    }

    #[cfg(unittest)]
    pub(super) fn cached_block_count(&self) -> usize {
        lock(&self.inner.entries).blocks.len()
    }

    pub(super) fn reclaim_unused(&self, limit: usize) -> usize {
        let mut entries = lock(&self.inner.entries);
        let reclaimed = entries.reclaim_unused(limit);
        self.update_reclaim_demand(&entries);
        reclaimed
    }

    /// Called under the index mutex after every insertion or removal. Serializing
    /// these stores with index mutations prevents an older scan from clearing a
    /// newer insertion's demand. Relaxed is sufficient: this hint publishes no
    /// buffer contents, and reclaim rechecks the index under its mutex.
    fn update_reclaim_demand(&self, entries: &MetadataCacheIndex) {
        let retained_blocks = METADATA_CACHE_RETAINED_BYTES / self.inner.device.block_size();
        self.inner
            .has_reclaim_demand
            .store(entries.blocks.len() > retained_blocks, Ordering::Relaxed);
    }

    /// Advances cache-owned reclaim demand at a mount access boundary.
    ///
    /// Inspect at most `scan_limit` slots only while resident payloads exceed
    /// the retention allowance. The caller retains mount protection after all
    /// allocation/prefetch critical sections have ended. Reads and insertions
    /// themselves never evict entries. This may leave demand for a later access;
    /// it neither scans the full cache nor promises a hard capacity bound.
    /// Below the allowance, an advisory atomic load skips the index mutex.
    /// A hit since the last scan earns one second chance within the same budget.
    pub(super) fn reclaim_for_pressure(&self, scan_limit: usize) -> usize {
        if scan_limit == 0 || !self.inner.has_reclaim_demand.load(Ordering::Relaxed) {
            return 0;
        }
        let mut entries = lock(&self.inner.entries);
        let retained_blocks = METADATA_CACHE_RETAINED_BYTES / self.inner.device.block_size();
        if entries.blocks.len() <= retained_blocks {
            return 0;
        }
        let reclaimed = entries.reclaim_unused_incremental(scan_limit);
        self.update_reclaim_demand(&entries);
        reclaimed
    }

    /// Reclaims only clean, unpinned entries named by a retired checkpoint.
    pub(super) fn reclaim_blocks(&self, blocks: &[FilesystemBlock]) -> usize {
        let mut entries = lock(&self.inner.entries);
        let mut reclaimed = 0;
        for block in blocks {
            let can_reclaim = entries
                .blocks
                .get(block)
                .is_some_and(MetadataCacheEntry::is_reclaimable);
            if can_reclaim {
                entries.blocks.remove(block);
                reclaimed += 1;
            }
        }
        self.update_reclaim_demand(&entries);
        reclaimed
    }

    /// Visits at most `scan_limit` entries, continuing after the previous visit.
    ///
    /// Advance across pinned and transaction-owned slots as well as reclaimed
    /// ones, so they cannot hide unrelated clean entries. Each sweep fixes its
    /// highest key before scanning; newly inserted higher keys cannot postpone
    /// wrapping and revisiting old pins. No entry is inspected twice per call.
    /// Referenced entries lose their second-chance bit instead of being evicted;
    /// this also consumes one inspection, without rescanning to fill the budget.
    /// Call only at a mount access boundary outside allocation/prefetch critical
    /// sections. This is a scan primitive, not a cache admission policy.
    #[cfg(unittest)]
    pub(super) fn reclaim_unused_incremental(&self, scan_limit: usize) -> usize {
        let mut entries = lock(&self.inner.entries);
        let reclaimed = entries.reclaim_unused_incremental(scan_limit);
        self.update_reclaim_demand(&entries);
        reclaimed
    }

    pub(super) fn invalidate_all(&self) {
        let mut entries = lock(&self.inner.entries);
        entries.blocks.clear();
        entries.reclaim_cursor = None;
        self.update_reclaim_demand(&entries);
    }

    pub(super) fn flush_device(&self) -> Ext4Result<()> {
        self.inner.device.flush()
    }

    #[cfg(unittest)]
    pub(super) fn buffer_state(&self, block: FilesystemBlock) -> Ext4Result<MetadataBufferState> {
        self.load_slot(block)?.state()
    }
}

impl MetadataCacheIndex {
    fn reclaim_unused(&mut self, limit: usize) -> usize {
        if limit == 0 {
            return 0;
        }
        if limit == usize::MAX {
            // A full inspection supersedes the old sweep even when pins prevent
            // any eviction.
            self.reclaim_cursor = None;
        }
        let candidates = self
            .blocks
            .iter()
            .filter_map(|(block, entry)| entry.is_reclaimable().then_some(*block))
            .take(limit)
            .collect::<alloc::vec::Vec<_>>();
        let reclaimed = candidates.len();
        for block in candidates {
            self.blocks.remove(&block);
        }
        reclaimed
    }

    fn reclaim_unused_incremental(&mut self, scan_limit: usize) -> usize {
        if scan_limit == 0 {
            return 0;
        }
        let (after, through) = match self.reclaim_cursor {
            Some(cursor) => (Excluded(cursor.after), cursor.through),
            None => {
                let Some((&last, _)) = self.blocks.last_key_value() else {
                    return 0;
                };
                (Unbounded, last)
            }
        };
        let candidates: alloc::vec::Vec<_> = self
            .blocks
            .range((after, Included(through)))
            .take(scan_limit)
            .map(|(block, _)| *block)
            .collect();
        self.reclaim_cursor = candidates.last().and_then(|&after| {
            (candidates.len() == scan_limit && after < through)
                .then_some(ReclaimCursor { after, through })
        });
        let mut reclaimed = 0;
        for block in candidates {
            let can_reclaim = self.blocks.get_mut(&block).is_some_and(|entry| {
                if core::mem::take(&mut entry.was_referenced) {
                    return false;
                }
                entry.is_reclaimable()
            });
            if can_reclaim {
                self.blocks.remove(&block);
                reclaimed += 1;
            }
        }
        reclaimed
    }
}

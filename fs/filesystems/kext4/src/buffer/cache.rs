// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{collections::BTreeMap, sync::Arc, vec};

#[cfg(test)]
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

struct MetadataBlockCacheInner {
    device: Arc<FilesystemDevice>,
    entries: Mutex<BTreeMap<FilesystemBlock, Arc<MetadataBufferSlot>>>,
}

/// Per-filesystem cache of immutable ext4 metadata blocks.
///
/// The index deliberately owns ready slots. This is an explicit retain and
/// reclaim cache: handles pin the immutable byte storage, while
/// `reclaim_unused` removes entries whose bytes have no external readers.
#[derive(Clone)]
pub(super) struct MetadataBlockCache {
    inner: Arc<MetadataBlockCacheInner>,
}

impl MetadataBlockCache {
    pub(super) fn new(device: Arc<FilesystemDevice>) -> Self {
        Self {
            inner: Arc::new(MetadataBlockCacheInner {
                device,
                entries: Mutex::new(BTreeMap::new()),
            }),
        }
    }

    fn load_slot(&self, block: FilesystemBlock) -> Ext4Result<Arc<MetadataBufferSlot>> {
        let (slot, is_loader) = {
            let mut entries = lock(&self.inner.entries);
            match entries.get(&block) {
                Some(slot) => (slot.clone(), false),
                None => {
                    let slot = Arc::new(MetadataBufferSlot::new());
                    entries.insert(block, slot.clone());
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
                    .get(&block)
                    .is_some_and(|cached| Arc::ptr_eq(cached, &slot))
                {
                    entries.remove(&block);
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
            match entries.get(&block) {
                Some(slot) => (slot.clone(), true),
                None => {
                    let slot = Arc::new(MetadataBufferSlot::new());
                    entries.insert(block, slot.clone());
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
            entries.get(&block).cloned()
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
                .get(&block)
                .is_some_and(|cached| Arc::ptr_eq(cached, &slot))
            {
                entries.remove(&block);
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

    #[cfg(test)]
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

    #[cfg(test)]
    pub(super) fn cached_block_count(&self) -> usize {
        lock(&self.inner.entries).len()
    }

    pub(super) fn reclaim_unused(&self, limit: usize) -> usize {
        if limit == 0 {
            return 0;
        }

        let mut entries = lock(&self.inner.entries);
        let candidates = entries
            .iter()
            .filter_map(|(block, slot)| {
                (Arc::strong_count(slot) == 1 && slot.is_reclaimable()).then_some(*block)
            })
            .take(limit)
            .collect::<alloc::vec::Vec<_>>();
        let reclaimed = candidates.len();
        for block in candidates {
            entries.remove(&block);
        }
        reclaimed
    }

    pub(super) fn invalidate_all(&self) {
        lock(&self.inner.entries).clear();
    }

    pub(super) fn flush_device(&self) -> Ext4Result<()> {
        self.inner.device.flush()
    }

    #[cfg(test)]
    pub(super) fn buffer_state(&self, block: FilesystemBlock) -> Ext4Result<MetadataBufferState> {
        self.load_slot(block)?.state()
    }
}

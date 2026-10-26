// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Allocator-owned exclusions for bitmap frees whose transaction is not durable.

use alloc::{
    collections::{BTreeMap, BTreeSet},
    vec::Vec,
};

use crate::{BlockGroupNumber, Ext4Error, Ext4Result, FilesystemBlock, jbd2::TransactionId};

/// Protected ranges indexed by allocation group and by freeing transaction.
/// The allocator mutex protects these indexes together with bitmap publication.
/// Cache rebuilds leave this state intact; only durable completion removes it.
#[derive(Debug, Default)]
pub(crate) struct PendingBlockFrees {
    groups: BTreeMap<BlockGroupNumber, BTreeMap<TransactionId, FreedBlocks>>,
    transactions: BTreeMap<TransactionId, BTreeSet<BlockGroupNumber>>,
    block_count: u64,
}

impl PendingBlockFrees {
    /// Counts bitmap-free blocks still excluded from allocation.
    pub(crate) fn block_count(&self) -> u64 {
        self.block_count
    }

    /// Whether completing this exact transaction would release exclusions.
    pub(crate) fn contains_transaction(&self, transaction: TransactionId) -> bool {
        self.transactions.contains_key(&transaction)
    }

    /// Register only after validating allocated bits, before publishing clears.
    pub(crate) fn insert(
        &mut self,
        group: BlockGroupNumber,
        transaction: TransactionId,
        first: FilesystemBlock,
        last: FilesystemBlock,
    ) -> Ext4Result<()> {
        let ranges = self
            .groups
            .entry(group)
            .or_default()
            .entry(transaction)
            .or_default();
        let before = ranges.block_count;
        ranges.insert_range(first, last)?;
        self.block_count += ranges.block_count - before;
        self.transactions
            .entry(transaction)
            .or_default()
            .insert(group);
        Ok(())
    }

    /// Appends clipped inclusive ranges from this group only.
    pub(crate) fn append_ranges(
        &self,
        group: BlockGroupNumber,
        first: FilesystemBlock,
        last: FilesystemBlock,
        output: &mut Vec<(FilesystemBlock, FilesystemBlock)>,
    ) {
        if let Some(transactions) = self.groups.get(&group) {
            for ranges in transactions.values() {
                ranges.append_intersections(first, last, output);
            }
        }
    }

    /// Called by filesystem journal glue after durable commit, outside JBD2 locks.
    pub(crate) fn complete(&mut self, transaction: TransactionId) {
        let Some(groups) = self.transactions.remove(&transaction) else {
            return;
        };
        for group in groups {
            if let Some(transactions) = self.groups.get_mut(&group) {
                if let Some(ranges) = transactions.remove(&transaction) {
                    self.block_count -= ranges.block_count;
                }
                if transactions.is_empty() {
                    self.groups.remove(&group);
                }
            }
        }
    }
}

/// Inclusive physical ranges withheld until their freeing transaction commits.
/// Storage scales with physical fragmentation; callers releasing an extent
/// register it once, before publishing any cleared bitmap bits.
#[derive(Debug, Default, Eq, PartialEq)]
struct FreedBlocks {
    ranges: BTreeMap<FilesystemBlock, FilesystemBlock>,
    block_count: u64,
}

impl FreedBlocks {
    fn insert_range(&mut self, first: FilesystemBlock, last: FilesystemBlock) -> Ext4Result<()> {
        if first > last {
            return Err(Ext4Error::OutOfBounds);
        }
        let first = self
            .ranges
            .range(..=first)
            .next_back()
            .filter(|&(_, end)| end.get().saturating_add(1) >= first.get())
            .map_or(first, |(&start, _)| start);
        let upper = FilesystemBlock::new(last.get().saturating_add(1));
        let mut merged_last = last;
        let mut replaced_blocks = 0;
        for (&start, &end) in self.ranges.range(first..=upper) {
            merged_last = merged_last.max(end);
            replaced_blocks += end.get() - start.get() + 1;
        }
        let merged_blocks = merged_last
            .get()
            .checked_sub(first.get())
            .and_then(|length| length.checked_add(1))
            .ok_or(Ext4Error::Overflow)?;
        let block_count = (self.block_count - replaced_blocks)
            .checked_add(merged_blocks)
            .ok_or(Ext4Error::Overflow)?;
        while let Some((&start, _)) = self.ranges.range(first..=upper).next() {
            self.ranges.remove(&start);
        }
        self.ranges.insert(first, merged_last);
        self.block_count = block_count;
        Ok(())
    }

    fn append_intersections(
        &self,
        first: FilesystemBlock,
        last: FilesystemBlock,
        ranges: &mut Vec<(FilesystemBlock, FilesystemBlock)>,
    ) {
        if let Some((&start, &end)) = self.ranges.range(..first).next_back()
            && end >= first
        {
            ranges.push((start.max(first), end.min(last)));
        }
        for (&start, &end) in self.ranges.range(first..=last) {
            ranges.push((start, end.min(last)));
        }
    }
}

#[cfg(unittest)]
unittest::path_mod_test!(tests, "../../ut-tests/balloc/pending_free.rs");

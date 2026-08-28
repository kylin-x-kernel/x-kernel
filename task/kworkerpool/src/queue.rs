// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Runnable and deferred entry storage for a worker pool.

use crate::{EntryKey, EntryOwner, EntryPayload, EntrySource};

/// Entry stored by one worker-pool instance.
///
/// `source` is an opaque runtime locator carried back at claim time. `owner` is
/// an opaque user-defined grouping key used for deferred promotion. `key` is an
/// opaque user-defined entry key used for removal. The pool never interprets
/// these values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PoolEntry {
    source: EntrySource,
    owner: EntryOwner,
    key: EntryKey,
    payload: EntryPayload,
}

impl PoolEntry {
    /// Creates a pool entry.
    pub const fn new(
        source: EntrySource,
        owner: EntryOwner,
        key: EntryKey,
        payload: EntryPayload,
    ) -> Self {
        Self {
            source,
            owner,
            key,
            payload,
        }
    }

    /// Returns the opaque runtime source identity.
    pub const fn source(self) -> EntrySource {
        self.source
    }

    /// Returns the opaque owner key.
    pub const fn owner(self) -> EntryOwner {
        self.owner
    }

    /// Returns the opaque entry key.
    pub const fn key(self) -> EntryKey {
        self.key
    }

    /// Consumes the entry and returns the payload.
    pub const fn into_payload(self) -> EntryPayload {
        self.payload
    }
}

/// Result of removing an entry from pool queues.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueRemoveResult {
    /// The entry came from the runnable FIFO.
    Runnable,
    /// The entry came from the deferred FIFO.
    Deferred,
}

/// Bounded runnable/deferred queue storage for one worker-pool instance.
///
/// Runnable and deferred entries are each stored in a single FIFO, and both
/// lanes share one total entry capacity. Owner keys are opaque grouping keys
/// used for mechanical promotion/removal scans; they do not create per-owner
/// storage.
pub struct PoolRunQueue<const ENTRY_CAP: usize> {
    runnable: EntryQueue<PoolEntry, ENTRY_CAP>,
    deferred: EntryQueue<PoolEntry, ENTRY_CAP>,
    len: usize,
}

impl<const ENTRY_CAP: usize> PoolRunQueue<ENTRY_CAP> {
    /// Creates an empty pool run queue.
    pub const fn new() -> Self {
        Self {
            runnable: EntryQueue::new(),
            deferred: EntryQueue::new(),
            len: 0,
        }
    }

    /// Returns total entries across runnable and deferred lanes.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns whether no entries are queued.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns current runnable FIFO length.
    pub fn runnable_len(&self) -> usize {
        self.runnable.len()
    }

    /// Returns current deferred FIFO length.
    pub fn deferred_len(&self) -> usize {
        self.deferred.len()
    }

    /// Returns runnable entries for one owner.
    pub fn runnable_len_for_owner(&self, owner: EntryOwner) -> usize {
        self.runnable.count_matching(|entry| entry.owner() == owner)
    }

    /// Returns deferred entries for one owner.
    pub fn deferred_len_for_owner(&self, owner: EntryOwner) -> usize {
        self.deferred.count_matching(|entry| entry.owner() == owner)
    }

    /// Pushes an entry directly into runnable FIFO.
    pub fn push_runnable(&mut self, entry: PoolEntry) -> Result<(), PoolEntry> {
        if self.len == ENTRY_CAP {
            return Err(entry);
        }
        self.runnable.push(entry)?;
        self.len += 1;
        Ok(())
    }

    /// Pushes an entry into the deferred FIFO.
    pub fn push_deferred(&mut self, entry: PoolEntry) -> Result<(), PoolEntry> {
        if self.len == ENTRY_CAP {
            return Err(entry);
        }
        self.deferred.push(entry)?;
        self.len += 1;
        Ok(())
    }

    /// Promotes up to `budget` deferred entries for one owner.
    pub fn promote_deferred(&mut self, owner: EntryOwner, budget: usize) -> usize {
        let mut promoted = 0;
        while promoted < budget && self.runnable.len() < ENTRY_CAP {
            if self.promote_one_deferred(owner).is_none() {
                break;
            }
            promoted += 1;
        }
        promoted
    }

    /// Promotes one deferred entry for `owner` and returns the moved entry.
    pub fn promote_one_deferred(&mut self, owner: EntryOwner) -> Option<PoolEntry> {
        if self.runnable.len() == ENTRY_CAP {
            return None;
        }
        let entry = self.deferred.remove_first(|entry| entry.owner() == owner)?;
        self.runnable
            .push(entry)
            .expect("moving within shared pool capacity should not fail");
        Some(entry)
    }

    /// Moves all runnable entries for `owner` into the deferred FIFO.
    ///
    /// This is a mechanical lane move; caller-owned policy decides when it is
    /// necessary.
    pub fn defer_runnable_for_owner(&mut self, owner: EntryOwner) -> usize {
        let mut moved = 0usize;
        while let Some(entry) = self.runnable.remove_first(|entry| entry.owner() == owner) {
            self.deferred
                .push(entry)
                .expect("moving within shared pool capacity should not fail");
            moved += 1;
        }
        moved
    }

    /// Pops the oldest runnable entry.
    pub fn pop_runnable(&mut self) -> Option<PoolEntry> {
        let entry = self.runnable.pop_front()?;
        self.len = self.len.saturating_sub(1);
        Some(entry)
    }

    /// Removes an entry by owner and key from runnable or deferred lanes.
    pub fn remove(
        &mut self,
        owner: EntryOwner,
        key: EntryKey,
    ) -> Option<(PoolEntry, QueueRemoveResult)> {
        if let Some(entry) = self
            .runnable
            .remove_first(|entry| entry.owner() == owner && entry.key() == key)
        {
            self.len = self.len.saturating_sub(1);
            return Some((entry, QueueRemoveResult::Runnable));
        }
        self.deferred
            .remove_first(|entry| entry.owner() == owner && entry.key() == key)
            .map(|entry| {
                self.len = self.len.saturating_sub(1);
                (entry, QueueRemoveResult::Deferred)
            })
    }

    /// Returns a mutable payload reference by owner and key.
    pub fn get_mut(&mut self, owner: EntryOwner, key: EntryKey) -> Option<&mut EntryPayload> {
        self.runnable
            .get_mut(|entry| entry.owner() == owner && entry.key() == key)
            .map(|entry| &mut entry.payload)
            .or_else(|| {
                self.deferred
                    .get_mut(|entry| entry.owner() == owner && entry.key() == key)
                    .map(|entry| &mut entry.payload)
            })
    }
}

impl<const ENTRY_CAP: usize> Default for PoolRunQueue<ENTRY_CAP> {
    fn default() -> Self {
        Self::new()
    }
}

struct EntryQueue<T, const CAP: usize> {
    entries: [Option<T>; CAP],
    head: usize,
    len: usize,
}

impl<T, const CAP: usize> EntryQueue<T, CAP> {
    const fn new() -> Self {
        Self {
            entries: [const { None }; CAP],
            head: 0,
            len: 0,
        }
    }

    fn len(&self) -> usize {
        self.len
    }

    fn physical_index(&self, logical_index: usize) -> usize {
        (self.head + logical_index) % CAP
    }

    fn push(&mut self, entry: T) -> Result<(), T> {
        if self.len == CAP {
            return Err(entry);
        }
        let index = self.physical_index(self.len);
        self.entries[index] = Some(entry);
        self.len += 1;
        Ok(())
    }

    fn pop_front(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        let index = self.head;
        let entry = self.entries[index].take();
        self.len -= 1;
        if self.len == 0 {
            self.head = 0;
        } else {
            self.head = self.physical_index(1);
        }
        entry
    }

    fn remove_first(&mut self, pred: impl Fn(&T) -> bool) -> Option<T> {
        let logical = (0..self.len).find(|index| {
            self.entries[self.physical_index(*index)]
                .as_ref()
                .is_some_and(&pred)
        })?;
        self.remove_logical(logical)
    }

    fn get_mut(&mut self, pred: impl Fn(&T) -> bool) -> Option<&mut T> {
        let logical = (0..self.len).find(|index| {
            self.entries[self.physical_index(*index)]
                .as_ref()
                .is_some_and(&pred)
        })?;
        let physical = self.physical_index(logical);
        self.entries[physical].as_mut()
    }

    fn count_matching(&self, pred: impl Fn(&T) -> bool) -> usize {
        (0..self.len)
            .filter(|index| {
                self.entries[self.physical_index(*index)]
                    .as_ref()
                    .is_some_and(&pred)
            })
            .count()
    }

    fn remove_logical(&mut self, logical_index: usize) -> Option<T> {
        let physical = self.physical_index(logical_index);
        let removed = self.entries[physical].take();

        for logical in logical_index..(self.len - 1) {
            let current = self.physical_index(logical);
            let next = self.physical_index(logical + 1);
            self.entries[current] = self.entries[next].take();
        }
        let tail = self.physical_index(self.len - 1);
        self.entries[tail] = None;
        self.len -= 1;
        if self.len == 0 {
            self.head = 0;
        }
        removed
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::{ops::Deref, sync::atomic::AtomicBool};

use hashbrown::HashMap;
use ksync::Mutex;

use crate::{FutexKey, WaitQueue};

/// The futex entry structure
pub struct FutexEntry {
    /// The wait queue associated with this futex.
    pub wq: WaitQueue,

    /// Used by robust list, indicates if the owner of this futex is dead.
    pub owner_dead: AtomicBool,
}

impl FutexEntry {
    fn new() -> Self {
        Self {
            wq: WaitQueue::new(),
            owner_dead: AtomicBool::new(false),
        }
    }
}

/// A table mapping memory addresses to futex wait queues.
pub struct FutexTable(Mutex<HashMap<usize, Arc<FutexEntry>>>);

impl FutexTable {
    /// Creates a new `FutexTable`.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self(Mutex::new(HashMap::new()))
    }

    /// Checks if the futex table is empty.
    pub fn is_empty(&self) -> bool {
        self.0.lock().is_empty()
    }

    /// Gets the wait queue associated with the given address.
    pub fn get(&self, key: &FutexKey) -> Option<FutexGuard<'_>> {
        let key = key.as_usize();
        let entry = self.0.lock().get(&key).cloned()?;
        Some(FutexGuard {
            table: self,
            key,
            inner: entry,
        })
    }

    /// Gets the wait queue associated with the given address, or inserts a new
    /// one if it doesn't exist.
    pub fn get_or_insert(&self, key: &FutexKey) -> FutexGuard<'_> {
        let key = key.as_usize();
        let mut table = self.0.lock();
        let entry = table
            .entry(key)
            .or_insert_with(|| Arc::new(FutexEntry::new()));
        FutexGuard {
            table: self,
            key,
            inner: entry.clone(),
        }
    }
}

#[doc(hidden)]
pub struct FutexGuard<'a> {
    table: &'a FutexTable,
    key: usize,
    inner: Arc<FutexEntry>,
}

impl Deref for FutexGuard<'_> {
    type Target = Arc<FutexEntry>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl Drop for FutexGuard<'_> {
    fn drop(&mut self) {
        if Arc::strong_count(&self.inner) <= 2 && self.inner.wq.is_empty() {
            self.table.0.lock().remove(&self.key);
        }
    }
}

#[cfg(unittest)]
mod tests {
    use unittest::def_test;

    use super::FutexTable;
    use crate::FutexKey;

    #[def_test]
    fn test_futextable_insert_drop() {
        let table = FutexTable::new();
        let key = FutexKey::Private { address: 0x1000 };
        {
            let _guard = table.get_or_insert(&key);
            assert!(table.get(&key).is_some());
            assert!(!table.is_empty());
        }
        assert!(table.get(&key).is_none());
        assert!(table.is_empty());
    }

    #[def_test]
    fn test_futextable_get_missing_and_persist_with_multiple_guards() {
        let table = FutexTable::new();
        let key = FutexKey::Private { address: 0x2000 };
        assert!(table.get(&key).is_none());

        let guard1 = table.get_or_insert(&key);
        let guard2 = table.get(&key).unwrap();
        drop(guard1);
        assert!(table.get(&key).is_some());
        drop(guard2);
        assert!(table.get(&key).is_none());
    }
}

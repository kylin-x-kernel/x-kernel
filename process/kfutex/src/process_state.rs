// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use hashbrown::HashMap;
use ksync::Mutex;
use lazy_static::lazy_static;
use memspace::VmObjectId;

use crate::{FutexKey, FutexTable, key::SharedRegionIdentity};

/// Process-owned futex state.
///
/// This object owns the process-private futex table and routes shared futexes
/// through the global shared-table cache maintained by `kfutex`.
pub struct ProcessFutexState {
    private_table: Arc<FutexTable>,
}

impl ProcessFutexState {
    /// Creates a new process-owned futex state.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            private_table: Arc::new(FutexTable::new()),
        }
    }

    /// Returns the futex table that should back the given key.
    pub fn table_for(&self, key: &FutexKey) -> Arc<FutexTable> {
        match key {
            FutexKey::Private { .. } => self.private_table.clone(),
            FutexKey::Shared { region, .. } => {
                let identity = match region {
                    SharedRegionIdentity::Anonymous(object) => *object,
                    SharedRegionIdentity::File(object) => *object,
                };
                SHARED_FUTEX_TABLES.lock().get_or_insert(identity)
            }
        }
    }
}

struct SharedFutexTables {
    map: HashMap<VmObjectId, Arc<FutexTable>>,
    operations: usize,
}

impl SharedFutexTables {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            operations: 0,
        }
    }

    fn get_or_insert(&mut self, key: VmObjectId) -> Arc<FutexTable> {
        self.operations += 1;
        if self.operations == 100 {
            self.operations = 0;
            self.map
                .retain(|_, table| Arc::strong_count(table) > 1 || !table.is_empty());
        }
        self.map
            .entry(key)
            .or_insert_with(|| Arc::new(FutexTable::new()))
            .clone()
    }
}

lazy_static! {
    static ref SHARED_FUTEX_TABLES: Mutex<SharedFutexTables> = Mutex::new(SharedFutexTables::new());
}

#[cfg(unittest)]
mod tests {
    use alloc::sync::Arc;

    use memspace::VmObjectId;
    use unittest::def_test;
    use vmobj::{AnonObjectId, FileObjectId};

    use super::SharedFutexTables;

    #[def_test]
    fn test_shared_futextables_get_or_insert_reuses_existing_table() {
        let mut tables = SharedFutexTables::new();
        let first = tables.get_or_insert(VmObjectId::File(FileObjectId::from_raw(0x1234)));
        let second = tables.get_or_insert(VmObjectId::File(FileObjectId::from_raw(0x1234)));

        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(tables.map.len(), 1);
        assert_eq!(tables.operations, 2);
    }

    #[def_test]
    fn test_shared_futextables_cleanup_drops_stale_entries_on_threshold() {
        let mut tables = SharedFutexTables::new();
        let stale = tables.get_or_insert(VmObjectId::Anon(AnonObjectId::from_raw(1)));
        drop(stale);

        tables.operations = 99;
        let fresh = tables.get_or_insert(VmObjectId::Anon(AnonObjectId::from_raw(2)));

        assert_eq!(tables.operations, 0);
        assert!(
            !tables
                .map
                .contains_key(&VmObjectId::Anon(AnonObjectId::from_raw(1)))
        );
        assert!(
            tables
                .map
                .contains_key(&VmObjectId::Anon(AnonObjectId::from_raw(2)))
        );
        assert_eq!(Arc::strong_count(&fresh), 2);
    }
}

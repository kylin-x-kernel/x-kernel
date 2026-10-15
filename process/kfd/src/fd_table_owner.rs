// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Binding ownership is separate from references used to inspect a table.

use alloc::sync::Arc;
use core::ops::Deref;

use ksync::{RwLock, RwLockReadGuard};

use crate::FdTable;

/// Owns the descriptors of a shared files binding, like Linux `files_struct`.
///
/// Only bindings retain this object. Temporary table references preserve the
/// table's address, but neither count as sharing nor delay descriptor teardown.
/// The last binding drains the table before its storage can be reclaimed.
pub struct FdTableOwner {
    table: Arc<RwLock<FdTable>>,
}

impl FdTableOwner {
    /// Creates an empty files owner.
    pub fn new_shared() -> Arc<Self> {
        Arc::new(Self {
            table: FdTable::new_shared(),
        })
    }

    /// Copies descriptors into an independent owner without copying POSIX locks.
    pub fn copy_from(owner: &Self) -> Arc<Self> {
        Arc::new(Self {
            table: FdTable::clone_shared_from(&owner.table),
        })
    }

    /// Borrows writable storage for no longer than this binding remains alive.
    pub fn table(&self) -> &RwLock<FdTable> {
        &self.table
    }

    /// Pins storage and identity without retaining descriptor ownership.
    pub fn reference(&self) -> FdTableRef {
        FdTableRef(self.table.clone())
    }
}

impl Drop for FdTableOwner {
    fn drop(&mut self) {
        loop {
            let next = {
                let mut table = self.table.write();
                let owner = table.lock_owner();
                let fd = table.ids().next();
                fd.and_then(|fd| table.remove(fd))
                    .map(|descriptor| (descriptor, owner))
            };
            let Some((descriptor, owner)) = next else {
                break;
            };
            // Close may reenter the table. Keep its identity alive, but release
            // its lock before flushing and removing the owner's POSIX locks.
            let _ = descriptor.close(owner);
        }
    }
}

/// Read-only access to a table whose descriptors may be closed by its last owner.
///
/// This capability pins storage and POSIX lock identity. It cannot modify or
/// repopulate the table; writable access requires borrowing [`FdTableOwner`].
#[derive(Clone)]
pub struct FdTableRef(Arc<RwLock<FdTable>>);

impl FdTableRef {
    /// Locks the table for inspection without acquiring binding ownership.
    ///
    /// The mapped guard does not expose the underlying writable lock.
    pub fn read(&self) -> impl Deref<Target = FdTable> + '_ {
        RwLockReadGuard::map(self.0.read(), |table| table)
    }
}

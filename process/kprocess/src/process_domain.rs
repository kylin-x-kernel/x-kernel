// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process-domain synchronization boundary.
//!
//! This is the x-kernel counterpart of Linux `tasklist_lock`. It serializes
//! compound updates and snapshots that must observe one consistent process
//! domain relation:
//!
//! - parent link and parent children membership;
//! - fork attachment to the process tree;
//! - exit-state transitions that affect wait/reap visibility;
//! - orphan reparenting;
//! - wait/autoreap detach from the current parent relation;
//! - task/process/process-group/session publication slot visibility.
//!
//! Like Linux, this boundary uses an IRQ-safe rwlock: short relation snapshots
//! take the read side, and mutations take the write side. Do not replace it
//! with `ksync::RwLock`: process exit, wait, and visibility paths cannot sleep
//! while holding the process-domain invariant.

use kspin::{SpinRwNoIrq, SpinRwNoIrqReadGuard, SpinRwNoIrqWriteGuard};

static PROCESS_DOMAIN_LOCK: ProcessDomainLock = ProcessDomainLock::new();

struct ProcessDomainLock {
    inner: SpinRwNoIrq<()>,
}

impl ProcessDomainLock {
    const fn new() -> Self {
        Self {
            inner: SpinRwNoIrq::new(()),
        }
    }

    fn read(&self) -> ProcessDomainReadGuard<'_> {
        self.inner.read()
    }

    fn write(&self) -> ProcessDomainWriteGuard<'_> {
        self.inner.write()
    }
}

/// Shared guard for process-domain relation snapshots.
pub(crate) type ProcessDomainReadGuard<'a> = SpinRwNoIrqReadGuard<'a, ()>;
/// Exclusive guard for process-domain transactions.
pub(crate) type ProcessDomainWriteGuard<'a> = SpinRwNoIrqWriteGuard<'a, ()>;

/// Acquires the process-domain transaction lock for relation snapshots.
pub(crate) fn read_lock() -> ProcessDomainReadGuard<'static> {
    PROCESS_DOMAIN_LOCK.read()
}

/// Acquires the process-domain transaction lock for mutation or relation snapshots.
pub(crate) fn write_lock() -> ProcessDomainWriteGuard<'static> {
    PROCESS_DOMAIN_LOCK.write()
}

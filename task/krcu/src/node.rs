// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Original-CPU ownership of preempted task readers.

use core::sync::atomic::{Ordering, fence};

use intrusive_collections::{LinkedList, UnsafeRef, intrusive_adapter};
use kspin::SpinNoIrq;

use crate::TaskReader;

intrusive_adapter!(ReaderAdapter = UnsafeRef<TaskReader>: TaskReader {
    link => intrusive_collections::LinkedListAtomicLink
});

/// A CPU's enrolled readers, serialized independently of every other CPU.
///
/// The list borrows embedded task storage; it does not allocate or own tasks.
/// Migration retains the original node until the outer scope exits.
pub struct ReaderNode {
    readers: SpinNoIrq<LinkedList<ReaderAdapter>>,
}

impl ReaderNode {
    /// Creates an empty node. Keep it stable while it has enrolled readers.
    pub const fn new() -> Self {
        Self {
            readers: SpinNoIrq::new(LinkedList::new(ReaderAdapter::NEW)),
        }
    }

    /// Records the outgoing task before the CPU can stop representing its read.
    /// Serial repeated calls, including after task migration, preserve existing
    /// membership. This does not support concurrent enrollment of one reader.
    ///
    /// # Safety
    ///
    /// Called by the scheduler for its outgoing current task, with IRQs and
    /// preemption excluded; never from NMI. No other scheduler may concurrently
    /// enroll this reader. Keep this node and reader at stable addresses through
    /// removal and all subsequent accesses. The host must retain tasks that
    /// abandon an active scope instead of destroying linked storage.
    /// Only that task may enter/exit its scopes; interrupt-context scopes are
    /// not supported by this enrollment implementation.
    pub unsafe fn enroll(&self, reader: &TaskReader) {
        if !reader.is_reading() || reader.is_enrolled() {
            return;
        }
        let mut readers = self.readers.lock();
        // The owner-node lock orders enrollment with future GP observations,
        // including when the underlying UP lock is only an exclusion guard.
        fence(Ordering::SeqCst);
        // SAFETY: the scheduler contract keeps embedded storage fixed and live
        // through removal. This outgoing reader is not in any other node.
        let borrowed = unsafe { UnsafeRef::from_raw(reader) };
        readers.push_front(borrowed);
        reader
            .owner
            .store(core::ptr::from_ref(self).cast_mut(), Ordering::Release);
    }

    /// # Safety
    /// The reader belongs to this node, which remains live and fixed. Only the
    /// task's outer exit may remove it; no concurrent enrollment/removal of this
    /// reader is allowed. Normal IRQ/preemption exclusion is acquired here.
    pub(super) unsafe fn remove(&self, reader: &TaskReader) {
        let mut readers = self.readers.lock();
        fence(Ordering::SeqCst);
        debug_assert!(
            core::ptr::eq(reader.owner.load(Ordering::Acquire), self),
            "RCU reader does not belong to this node"
        );
        // SAFETY: owner publication identifies this exact list; the caller is
        // the sole outer exit and this lock serializes all list mutations.
        let mut cursor = unsafe { readers.cursor_mut_from_ptr(reader) };
        let removed = cursor.remove();
        assert!(
            removed.is_some(),
            "enrolled RCU reader missing from owner node"
        );
        reader.owner.store(core::ptr::null_mut(), Ordering::Release);
    }

    /// Number of enrolled readers for controlled scheduler tests, not a GP test.
    #[cfg(any(test, unittest))]
    pub fn reader_count(&self) -> usize {
        self.readers.lock().iter().count()
    }
}

#[cfg(any(test, unittest))]
#[path = "../ut-tests/node.rs"]
mod tests;

impl Default for ReaderNode {
    fn default() -> Self {
        Self::new()
    }
}

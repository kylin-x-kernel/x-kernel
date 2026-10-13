// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{
    marker::PhantomData,
    sync::atomic::{AtomicPtr, AtomicUsize, Ordering},
};

use crate::ReaderNode;

/// Read nesting owned by one task, independent of its current CPU.
///
/// Scheduler integration must use the current task's instance, not a temporary
/// counter. No grace-period completion can be inferred from this state alone.
#[derive(Default)]
pub struct TaskReader {
    nesting: AtomicUsize,
    pub(super) link: intrusive_collections::LinkedListAtomicLink,
    pub(super) owner: AtomicPtr<ReaderNode>,
}

impl TaskReader {
    /// Constructs an inactive reader before the task can execute.
    pub const fn new() -> Self {
        Self {
            nesting: AtomicUsize::new(0),
            link: intrusive_collections::LinkedListAtomicLink::new(),
            owner: AtomicPtr::new(core::ptr::null_mut()),
        }
    }

    /// Reports explicit read participation without affecting preemptibility.
    pub fn is_reading(&self) -> bool {
        self.nesting.load(Ordering::Acquire) != 0
    }

    /// Reports whether an original CPU node still borrows this task's storage.
    pub fn is_enrolled(&self) -> bool {
        !self.owner.load(Ordering::Acquire).is_null()
    }

    /// Tests the original enrollment identity without dereferencing a raw owner.
    #[cfg(any(test, unittest))]
    pub fn is_enrolled_on(&self, node: &ReaderNode) -> bool {
        core::ptr::eq(self.owner.load(Ordering::Acquire), node)
    }

    /// Runs a scope with a borrowed, non-transferable read token.
    ///
    /// This is a host integration primitive, not a complete RCU read API.
    /// The host uses the current task's state and forbids voluntary blocking.
    /// The token cannot escape through the return value or be moved into another
    /// task. Normal return (including an error result) balances nesting. An
    /// execution path that abandons its stack leaves participation recorded.
    /// Once enrolled, only the owning task may enter/exit scopes, as required
    /// by [`ReaderNode::enroll`]. IRQ/NMI scope use is not supported.
    ///
    /// A token borrow cannot outlive the scope:
    ///
    /// ```compile_fail
    /// let reader = krcu::TaskReader::new();
    /// let escaped = reader.with_read(|guard| guard);
    /// drop(escaped);
    /// ```
    ///
    /// Nor can a caller extract and forget the owning guard:
    ///
    /// ```compile_fail
    /// krcu::TaskReader::new().with_read(|guard| core::mem::forget(*guard));
    /// ```
    ///
    /// Even a scoped thread cannot receive the borrowed token:
    ///
    /// ```compile_fail
    /// krcu::TaskReader::new().with_read(|guard| {
    ///     std::thread::scope(|scope| {
    ///         scope.spawn(move || { let _token = guard; });
    ///     });
    /// });
    /// ```
    ///
    /// # Panics
    ///
    /// Panics before entering the callback if nesting would overflow. Wrapping
    /// to zero would incorrectly represent an active reader as inactive.
    pub fn with_read<R>(&self, read_fn: impl for<'scope> FnOnce(&'scope ReadGuard<'_>) -> R) -> R {
        assert!(self.try_enter(), "RCU read nesting overflow");
        let guard = ReadGuard {
            reader: self,
            task_local: PhantomData,
        };
        read_fn(&guard)
    }

    fn try_enter(&self) -> bool {
        // Atomic RMW checks and updates shared state observed by the scheduler;
        // Acquire keeps scope operations after entry, Release publishes nesting.
        self.nesting
            .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |depth| {
                depth.checked_add(1)
            })
            .is_ok()
    }

    fn try_leave(&self) -> Option<usize> {
        // Scope accesses precede the exit observed by scheduler inspection.
        self.nesting
            .fetch_update(Ordering::Release, Ordering::Relaxed, |depth| {
                depth.checked_sub(1)
            })
            .ok()
            .map(|previous| previous - 1)
    }
}

/// Borrowed proof of a scope's participation in its task's reader accounting.
///
/// Only [`TaskReader::with_read`] constructs and owns this guard. Callers cannot
/// extract it, suppress its destruction or transfer its borrow to another task.
/// It provides no published-data access until the reclamation protocol exists.
pub struct ReadGuard<'reader> {
    reader: &'reader TaskReader,
    task_local: PhantomData<*mut ()>,
}

impl Drop for ReadGuard<'_> {
    fn drop(&mut self) {
        let depth = self.reader.try_leave();
        assert!(depth.is_some(), "RCU read nesting underflow");
        if depth == Some(0) {
            let owner = self.reader.owner.load(Ordering::Acquire);
            if !owner.is_null() {
                // SAFETY: enroll's host contract retains the node and reader at
                // stable addresses. Only this task exits its scope; a scheduler
                // cannot enroll the zero-depth task before removal completes.
                let node = unsafe { &*owner };
                // SAFETY: the non-null owner names this reader's unique list;
                // this is its outer exit, and remove supplies IRQ-safe locking.
                unsafe { node.remove(self.reader) };
            }
        }
    }
}

#[cfg(any(test, unittest))]
#[path = "../ut-tests/reader.rs"]
mod tests;

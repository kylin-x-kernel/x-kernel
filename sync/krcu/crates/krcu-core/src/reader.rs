// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{
    marker::PhantomData,
    sync::atomic::{AtomicPtr, AtomicUsize, Ordering},
};

use crate::ReaderNode;

/// Result of discharging a task's read participation at a safe boundary.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReaderCleanupOutcome {
    /// Cleanup discharged nesting or enrollment, rather than an already idle reader.
    pub had_participation: bool,
    /// Removal satisfied the original node's last reader obligation after CPU observation.
    /// This is only a service notification hint, never permission to reclaim.
    pub needs_progress: bool,
}

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

    /// Discharges abandoned scopes after the host permanently stops this task.
    ///
    /// Returns residual participation and a progress hint. Uses the same unlink
    /// path as an ordinary outer exit; does not certify a CPU quiescent state
    /// or run reclamation callbacks.
    ///
    /// # Safety
    ///
    /// The task must never resume: its stack is inactive and it is no longer
    /// current on any CPU. No scope guard may subsequently run, and no task,
    /// IRQ or NMI may enter, leave or enroll this reader. Keep the reader and
    /// its owner node fixed and live through this call. Ordinary IRQs and
    /// preemption must be excluded; this must not run in NMI context.
    pub unsafe fn finish_task_exit(&self) -> ReaderCleanupOutcome {
        let was_reading = self.nesting.swap(0, Ordering::AcqRel) != 0;
        let mut outcome = self.remove_enrollment();
        outcome.had_participation |= was_reading;
        outcome
    }

    fn remove_enrollment(&self) -> ReaderCleanupOutcome {
        if !self.is_enrolled() {
            return ReaderCleanupOutcome::default();
        }
        // A switch after nesting reaches zero may finish deferred cleanup.
        // Re-read ownership under exclusion before borrowing the original list.
        let _guard = kspin::NoPreemptIrqSave::new();
        let owner = self.owner.load(Ordering::Acquire);
        if !owner.is_null() {
            // SAFETY: enrollment retains stable node/reader storage through
            // removal. Only the owning task's outer exit or its exclusive
            // deferred cleanup or post-switch terminal cleanup reaches here.
            let node = unsafe { &*owner };
            // SAFETY: the published owner names this reader's unique list;
            // no competing scope exit or enrollment is permitted for it.
            let needs_progress = unsafe { node.remove(self) };
            return ReaderCleanupOutcome {
                had_participation: true,
                needs_progress,
            };
        }
        ReaderCleanupOutcome::default()
    }

    /// Unlinks a finished reader, retaining membership if a scope has reentered.
    ///
    /// Reports removed membership and whether it made the original node ready.
    /// Retained active scopes return an empty outcome. Does not certify CPU quiescence,
    /// wake the grace-period service or invoke callbacks.
    ///
    /// # Safety
    ///
    /// Operate on the current task's reader or an exclusively stopped fixture.
    /// Ordinary IRQs and preemption must remain excluded through this call;
    /// this must not run in NMI or while holding the owner node's lock. Nested
    /// NMI scopes must use only `with_read_deferred` and return balanced, without
    /// changing enrollment. Retain stable reader/node storage through removal.
    pub unsafe fn finish_deferred_cleanup(&self) -> ReaderCleanupOutcome {
        if self.is_reading() {
            return ReaderCleanupOutcome::default();
        }
        self.remove_enrollment()
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
    /// execution path that abandons its stack leaves participation recorded
    /// until the host performs [`Self::finish_task_exit`] after permanent exit.
    /// Once enrolled, only the owning task may enter/exit scopes, as required
    /// by [`ReaderNode::enroll`]. IRQ/NMI scope use is not supported.
    ///
    /// Outer exit unlinks enrollment but does not notify a grace-period service.
    /// The host must independently drive progress. Hosts needing the last-reader
    /// notification use [`Self::with_read_deferred`] and consume the outcome of
    /// [`Self::finish_deferred_cleanup`] at a safe boundary.
    ///
    /// A token borrow cannot outlive the scope:
    ///
    /// ```compile_fail
    /// let reader = krcu_core::TaskReader::new();
    /// let escaped = reader.with_read(|guard| guard);
    /// drop(escaped);
    /// ```
    ///
    /// Nor can a caller extract and forget the owning guard:
    ///
    /// ```compile_fail
    /// krcu_core::TaskReader::new().with_read(|guard| core::mem::forget(*guard));
    /// ```
    ///
    /// Even a scoped thread cannot receive the borrowed token:
    ///
    /// ```compile_fail
    /// krcu_core::TaskReader::new().with_read(|guard| {
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
            can_unlink_on_exit: true,
            task_local: PhantomData,
        };
        read_fn(&guard)
    }

    /// Runs a scope whose exit only updates nesting, never the enrollment list.
    ///
    /// Zero nesting with retained enrollment leaves the old period blocked until
    /// a safe cleanup boundary. No allocation, lock, wakeup or callback is used
    /// by this scope's entry/exit accounting. The token has the same borrowing
    /// restrictions as [`Self::with_read`]. This is not general IRQ admission.
    ///
    /// # Safety
    ///
    /// Use the current task's reader, with strictly nested execution on its CPU,
    /// or an exclusive fixture. No other CPU may concurrently enter/exit this
    /// reader. The callback must not block. In interrupt context it must not
    /// change enrollment or use ordinary `with_read`. Keep reader/node storage
    /// stable and alive until a normal outer exit, `finish_deferred_cleanup`
    /// or permanent task-exit cleanup removes any retained enrollment. The host
    /// must arrange that cleanup in a permitted context to guarantee progress.
    ///
    /// # Panics
    ///
    /// Panics before the callback if nesting would overflow.
    pub unsafe fn with_read_deferred<R>(
        &self,
        read_fn: impl for<'scope> FnOnce(&'scope ReadGuard<'_>) -> R,
    ) -> R {
        assert!(self.try_enter(), "RCU read nesting overflow");
        let guard = ReadGuard {
            reader: self,
            can_unlink_on_exit: false,
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
/// Production code constructs and owns this guard through [`TaskReader::with_read`]
/// or [`TaskReader::with_read_deferred`].
/// Callers of that interface cannot extract it, suppress its destruction or
/// transfer its borrow to another task. Private test fixtures construct guards
/// directly to model overlapping task scopes with independently controlled exits.
/// It provides no published-data access until the reclamation protocol exists.
pub struct ReadGuard<'reader> {
    reader: &'reader TaskReader,
    can_unlink_on_exit: bool,
    task_local: PhantomData<*mut ()>,
}

impl Drop for ReadGuard<'_> {
    fn drop(&mut self) {
        let depth = self.reader.try_leave();
        assert!(depth.is_some(), "RCU read nesting underflow");
        if depth == Some(0) && self.can_unlink_on_exit {
            // This primitive has no host notifier; its caller drives progress.
            // Deferred host integration consumes the hint at its cleanup boundary.
            self.reader.remove_enrollment();
        }
    }
}

unittest::path_mod_test!(tests, "../ut-tests/reader.rs");

unittest::path_mod_test!(tests_grace_period, "../ut-tests/grace_period.rs");

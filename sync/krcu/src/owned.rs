// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Owning publication, scoped borrowing and task-context synchronous replacement.

use alloc::boxed::Box;
use core::{
    marker::PhantomData,
    sync::atomic::{AtomicPtr, Ordering},
};

use kerrno::KError;

use super::synchronize::with_prepared_grace_period;
use crate::ReadScope;

/// Owns the currently published allocation; replacements privately retain old ones.
///
/// Shared access requires `T: Send + Sync`. Readers receive only scoped shared
/// borrows, never an independently owned pointer. Concurrent replacements may
/// overlap; each returns its own displaced allocation after a grace period.
/// This is not a compare-and-swap update or a lock for interior mutable state.
/// Final destruction requires exclusive ownership and drops the current value
/// directly in the dropping context, without another grace period.
pub struct Rcu<T> {
    value: AtomicPtr<T>,
    _owner: PhantomData<Box<T>>,
}

// SAFETY: shared readers only receive &T, hence Sync. A replacement can return
// an allocation published by another task, hence Send even for shared access.
// Atomic exchange uniquely assigns each old owner; no mutable borrow is exposed.
unsafe impl<T: Send + Sync> Sync for Rcu<T> {}

impl<T> Rcu<T> {
    /// Takes ownership of an initialized allocation without waiting or allocating.
    pub fn new(value: Box<T>) -> Self {
        Self {
            value: AtomicPtr::new(Box::into_raw(value)),
            _owner: PhantomData,
        }
    }

    /// Runs a task-context reader against one published allocation.
    ///
    /// Only owned results may escape the closure; a borrow of the allocation
    /// cannot outlive this scope. Nested reads are allowed. The closure must
    /// not voluntarily sleep or yield, though involuntary preemption and
    /// migration remain allowed. This does not serialize updates to `T`'s
    /// interior mutable state or provide a multi-container snapshot.
    ///
    /// # Panics
    /// Panics outside watched task context, including interrupt handlers and
    /// BH-disabled task context. The boot-owned RCU service must remain live.
    ///
    /// # Borrow Lifetime
    ///
    /// A protected reference cannot escape:
    ///
    /// ```compile_fail
    /// # extern crate alloc;
    /// use alloc::boxed::Box;
    /// let value = krcu::Rcu::new(Box::new(42_u64));
    /// let escaped = value.with_read(|current| current);
    /// let _ = *escaped;
    /// ```
    pub fn with_read<R>(&self, read_fn: impl for<'scope> FnOnce(&'scope T) -> R) -> R {
        crate::with_read(|scope| read_fn(self.read(scope)))
    }

    /// Borrows the current value within a kernel RCU scope.
    ///
    /// The reference borrows both this container and the scope. Repeated reads
    /// may see different publications; keep the first reference when one version
    /// is needed throughout an operation. Use [`crate::with_read`] in task context
    /// or [`crate::with_interrupt_read`] in an initialized interrupt handler.
    /// Standalone core reader tokens are not accepted.
    ///
    /// ```compile_fail
    /// # extern crate alloc;
    /// let value = krcu::Rcu::new(alloc::boxed::Box::new(42));
    /// let escaped = krcu::with_read(|scope| value.read(scope));
    /// let _ = *escaped;
    /// ```
    pub fn read<'scope>(&'scope self, _scope: &'scope ReadScope<'_>) -> &'scope T {
        let value = self.value.load(Ordering::Acquire);
        // SAFETY: this container always owns a non-null initialized allocation.
        // A removed allocation stays raw-owned until its GP completes. The
        // host-issued scope retains CPU/enrollment obligations; an interrupt borrow ends
        // before its CPU can report a later quiescent boundary. The lifetime also
        // prevents final container destruction while this reference is borrowed.
        unsafe { &*value }
    }

    /// Returns the old allocation only after its protected readers have exited.
    /// On preparation failure returns the unused new allocation without exchange.
    /// The caller must hold no lock needed by a reader or the progress service.
    ///
    /// # Errors
    /// Returns `InvalidInput` outside sleepable task context, including inside
    /// a read scope; `BadState` before RCU service readiness; or `NoMemory` on
    /// fallible wait registration. The error includes the unused replacement.
    /// Other allocations follow the kernel's fatal OOM policy. After publication
    /// the wait cannot be interrupted; return proves old readers have finished,
    /// not that unrelated callbacks or their downstream work have finished.
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: preparation either rejects before \
                  invoking the body or invokes it exactly once without later failure"
    )]
    pub fn replace_sync(&self, new_value: Box<T>) -> Result<Box<T>, (KError, Box<T>)> {
        // Retain the unused owner outside the body until all preparation succeeds.
        let mut new_value = Some(new_value);
        with_prepared_grace_period(|wait| {
            let new_value = new_value.take().expect("replacement body runs once");
            let old = self.value.swap(Box::into_raw(new_value), Ordering::AcqRel);
            // Do not reconstruct a Box yet: old readers may still hold shared
            // borrows. A nonreturning wait leaks this raw owner instead of freeing
            // it early, including if unwinding is introduced in a host environment.
            wait.submit_and_wait();
            // SAFETY: the exchange gave this operation unique ownership, and its
            // subsequent GP ended all protected borrows of this removed allocation.
            // No independent publication or raw ownership API is exposed.
            unsafe { Box::from_raw(old) }
        })
        .map_err(|error| {
            (
                error,
                new_value.expect("preparation failure preserves owner"),
            )
        })
    }
}

impl<T> Drop for Rcu<T> {
    fn drop(&mut self) {
        // SAFETY: exclusive final ownership excludes read references and pending
        // replacements, both of which borrow self. The current pointer still
        // represents exactly one Box owner, distinct from every returned value.
        drop(unsafe { Box::from_raw(*self.value.get_mut()) });
    }
}

unittest::path_mod_test!(tests, "../ut-tests/rcu_owned.rs");

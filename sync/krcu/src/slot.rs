// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Optional owning publication with split-phase, task-bound retirement.

use core::{
    marker::PhantomData,
    ptr,
    sync::atomic::{AtomicPtr, Ordering},
};

use kalloc_types::KBox;
use kerrno::KResult;

use super::synchronize::{PreparedGracePeriodWait, with_prepared_grace_period};
use crate::ReadScope;

/// An optionally published owner, borrowed only inside a kernel RCU scope.
///
/// Empty construction and first publication require no RCU service. Readers
/// still require initialized task/interrupt participation. Writers must separately
/// serialize any surrounding lifecycle state; this slot provides ownership, not
/// device shutdown, admission closure or a multiple-object transaction.
/// Final exclusive destruction drops the current owner in the dropping context.
///
/// ```no_run
/// use kalloc_types::KBox;
/// use krcu::RcuSlot;
///
/// fn publish_and_remove() -> kerrno::KResult<()> {
///     let slot = RcuSlot::new();
///     let value = KBox::new_nowait(42_u64)?;
///     // A private fresh slot cannot have a competing publisher.
///     assert!(slot.try_publish(value).is_ok());
///     let old = slot.with_prepared_take(|prepared| {
///         let retired = prepared.withdraw();
///         // SAFETY: unchanged watched, sleepable preparing task; no reader
///         // or progress lock is held and the boot RCU service remains live.
///         unsafe { retired.reclaim() }
///     })?;
///     drop(old); // Destruction runs here, not in a callback.
///     Ok(())
/// }
/// ```
pub struct RcuSlot<T> {
    value: AtomicPtr<T>,
    owner: PhantomData<KBox<T>>,
}

// SAFETY: readers obtain only scoped &T (Sync); a withdrawing task can recover
// an owner published by another task (Send). Atomic operations uniquely assign
// ownership and old pointers are never reconstructed before their grace period.
unsafe impl<T: Send + Sync> Sync for RcuSlot<T> {}

impl<T> RcuSlot<T> {
    /// Constructs an empty slot without allocation or service access.
    pub const fn new() -> Self {
        Self {
            value: AtomicPtr::new(ptr::null_mut()),
            owner: PhantomData,
        }
    }

    /// Publishes only into an empty slot, without allocating or waiting.
    ///
    /// An occupied slot returns the offered owner unchanged. Success transfers
    /// ownership to the slot; it does not enable a device or authorize readers
    /// outside a host-issued RCU scope.
    pub fn try_publish(&self, value: KBox<T>) -> Result<(), KBox<T>> {
        let raw = KBox::into_raw(value);
        match self.value.compare_exchange(
            ptr::null_mut(),
            raw,
            Ordering::Release,
            Ordering::Relaxed,
        ) {
            Ok(_) => Ok(()),
            Err(_) => {
                // SAFETY: failed exchange never published this unique allocation.
                Err(unsafe { KBox::from_raw(raw) })
            }
        }
    }

    /// Borrows the current publication, retaining both slot and scope lifetimes.
    ///
    /// Keep this reference for the complete operation, including controller
    /// completion when used by an IRQ owner. Repeated reads may see new values.
    ///
    /// ```compile_fail
    /// let slot = krcu::RcuSlot::<u64>::new();
    /// let escaped = krcu::with_read(|scope| slot.read(scope));
    /// let _ = escaped.unwrap();
    /// ```
    pub fn read<'scope>(&'scope self, _scope: &'scope ReadScope<'_>) -> Option<&'scope T> {
        let raw = self.value.load(Ordering::Acquire);
        // SAFETY: nonnull publications own initialized T. Withdrawal retains raw
        // ownership through a subsequent GP; the scope protects this borrow and
        // borrowing self excludes final slot destruction.
        unsafe { raw.as_ref() }
    }

    /// Prepares task waiting and callback storage before invoking the writer.
    ///
    /// The writer may withdraw under its own short lock, then must restore
    /// sleepable context before reclaiming. Preparation does not close device
    /// admission or prevent another publisher. Callback allocation currently
    /// follows the kernel fatal-OOM policy. Cancelling preparation changes nothing.
    ///
    /// # Errors
    /// Returns InvalidInput outside sleepable task context or BadState before
    /// service readiness, without invoking the writer or changing the slot.
    ///
    /// The prepared waiter cannot escape its provider's stack scope:
    ///
    /// ```compile_fail
    /// let slot = krcu::RcuSlot::<u64>::new();
    /// let escaped = slot.with_prepared_take(|prepared| prepared.withdraw());
    /// drop(escaped);
    /// ```
    pub fn with_prepared_take<'slot, R>(
        &'slot self,
        writer: impl FnOnce(PreparedRcuTake<'slot, '_, T>) -> R,
    ) -> KResult<R> {
        with_prepared_grace_period(|wait| {
            writer(PreparedRcuTake {
                slot: self,
                wait,
                task_local: PhantomData,
            })
        })
    }
}

impl<T> Default for RcuSlot<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Drop for RcuSlot<T> {
    fn drop(&mut self) {
        let raw = *self.value.get_mut();
        if !raw.is_null() {
            // SAFETY: exclusive final ownership excludes scoped readers and
            // withdrawals, both of which borrow self. This is the current KBox.
            drop(unsafe { KBox::from_raw(raw) });
        }
    }
}

/// Single-use withdrawal preparation bound to the preparing task and slot.
///
/// Created only by [`RcuSlot::with_prepared_take`]. Dropping it cancels the
/// private wait marker without modifying publication.
pub struct PreparedRcuTake<'slot, 'wait, T> {
    slot: &'slot RcuSlot<T>,
    wait: PreparedGracePeriodWait<'wait>,
    task_local: PhantomData<*mut ()>,
}

impl<'wait, T> PreparedRcuTake<'_, 'wait, T> {
    /// Atomically clears the slot without allocation, waiting or destruction.
    ///
    /// The returned owner is opaque until its post-withdrawal grace period ends.
    /// Caller-held writer exclusion may span this operation, but not reclamation.
    pub fn withdraw(self) -> RetiredRcuValue<'wait, T> {
        let raw = self.slot.value.swap(ptr::null_mut(), Ordering::AcqRel);
        RetiredRcuValue {
            raw,
            wait: self.wait,
            task_local: PhantomData,
        }
    }
}

/// Unique withdrawn ownership, not yet available for access or destruction.
///
/// Dropping or forgetting this object deliberately leaks its payload, rather
/// than freeing data still observable by readers. The unused wait marker is
/// cancelled normally. Reclamation must remain on the preparing task.
///
/// ```compile_fail
/// fn require_send<T: Send>() {}
/// require_send::<krcu::RetiredRcuValue<'static, u64>>();
/// ```
#[must_use = "reclaim after releasing writer locks; abandonment leaks the withdrawn owner"]
pub struct RetiredRcuValue<'wait, T> {
    raw: *mut T,
    wait: PreparedGracePeriodWait<'wait>,
    task_local: PhantomData<*mut ()>,
}

impl<T> RetiredRcuValue<'_, T> {
    /// Waits for old readers and returns their allocation in task context.
    ///
    /// No payload destructor runs in the RCU callback. An empty withdrawal
    /// returns None without submitting a marker or waiting for a grace period.
    ///
    /// # Safety
    /// Restore the preparing task's watched, sleepable context before calling:
    /// no read scope, IRQ/preemption exclusion or lock needed by readers/progress
    /// may remain held. Keep the permanent service and participants available.
    /// The wait is noninterruptible; no owner may be recovered on abandonment.
    pub unsafe fn reclaim(self) -> Option<KBox<T>> {
        if self.raw.is_null() {
            return None;
        }
        // SAFETY: reclaim's caller restores the preparing task's watched,
        // sleepable context after withdrawal and keeps readers/progress unblocked.
        unsafe { self.wait.submit_and_wait() };
        // SAFETY: atomic withdrawal assigned this unique raw owner, and the
        // subsequent GP ended all protected borrows. No raw owner was exposed.
        Some(unsafe { KBox::from_raw(self.raw) })
    }
}

unittest::path_mod_test!(tests, "../ut-tests/rcu_slot.rs");

#[cfg(test)]
#[path = "../tests/host/slot.rs"]
mod host_tests;

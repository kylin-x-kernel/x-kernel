// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Task-owned removal, separate from callback-owned asynchronous retirement.

use core::marker::PhantomData;

use kalloc_types::KBox;
use kerrno::KResult;
use krcu::{PreparedGracePeriodWait, with_prepared_grace_period};

use super::{Node, RcuList};

impl<T: Send + Sync + 'static> RcuList<T> {
    /// Prepares a single removal before entering consumer writer exclusion.
    ///
    /// The body may withdraw under a short lock, then reclaim after restoring
    /// the preparing task's sleepable context. No payload is destroyed by an
    /// RCU callback. This does not serialize the consumer's larger transaction
    /// or wait for operations which retain their own payload owners.
    /// Dropping the preparation without withdrawal leaves membership unchanged.
    /// Callback marker allocation follows krcu's fatal-OOM policy.
    ///
    /// # Errors
    /// InvalidInput rejects non-sleepable context and BadState rejects an
    /// unavailable service, before the body or any list mutation.
    ///
    /// The prepared retirement cannot escape the preparing stack:
    ///
    /// ```compile_fail
    /// let list = krcu_list::RcuList::<usize>::new();
    /// let escaped = list.with_prepared_take(|take| {
    ///     // SAFETY: assume initialized, non-reentrant task context.
    ///     unsafe { take.withdraw_first(|_| true) }
    /// });
    /// drop(escaped);
    /// ```
    pub fn with_prepared_take<'list, R>(
        &'list self,
        body: impl FnOnce(PreparedListTake<'list, '_, T>) -> R,
    ) -> KResult<R> {
        with_prepared_grace_period(|wait| {
            body(PreparedListTake {
                list: self,
                wait,
                task_local: PhantomData,
            })
        })
    }
}

/// Single-use removal preparation bound to a list and the preparing task.
///
/// Constructed only by [`RcuList::with_prepared_take`]. Cancellation does not
/// unlink an entry or invoke its destructor.
pub struct PreparedListTake<'list, 'wait, T: Send + Sync + 'static> {
    list: &'list RcuList<T>,
    wait: PreparedGracePeriodWait<'wait>,
    task_local: PhantomData<*mut ()>,
}

impl<'wait, T: Send + Sync + 'static> PreparedListTake<'_, 'wait, T> {
    /// Unlinks the first match without allocation, waiting or destruction.
    ///
    /// Preserves the removed forward link for existing readers. The returned
    /// handle exposes neither the payload nor mutable links until reclamation.
    /// Consumer short locks may span this operation, but not reclamation.
    ///
    /// # Safety
    /// Call from the preparing task, not IRQ, softirq or NMI. The predicate
    /// must not allocate, block, reenter this list's writers, or leave execution
    /// context changed. Hold no scheduler/RCU bookkeeping lock.
    pub unsafe fn withdraw_first(
        self,
        mut predicate: impl FnMut(&T) -> bool,
    ) -> RetiredListEntry<'wait, T> {
        let raw = self.list.withdraw(&mut predicate);
        RetiredListEntry {
            raw,
            wait: self.wait,
            task_local: PhantomData,
        }
    }
}

/// Opaque detached ownership awaiting a post-unlink grace period.
///
/// Dropping this handle deliberately leaks a matched entry rather than freeing
/// storage still observable by readers, while cancelling the unused wait marker.
/// Forgetting it leaks both resources. Reclamation never follows detached links;
/// the enclosing prepared-take call still borrows the list until its body returns.
///
/// ```compile_fail
/// fn require_send<T: Send>() {}
/// require_send::<krcu_list::RetiredListEntry<'static, usize>>();
/// ```
#[must_use = "reclaim after releasing writer locks; abandonment leaks the detached entry"]
pub struct RetiredListEntry<'wait, T: Send + Sync + 'static> {
    raw: *mut Node<T>,
    wait: PreparedGracePeriodWait<'wait>,
    task_local: PhantomData<*mut ()>,
}

impl<T: Send + Sync + 'static> RetiredListEntry<'_, T> {
    /// Whether withdrawal matched an entry, without accessing its payload.
    pub fn is_some(&self) -> bool {
        !self.raw.is_null()
    }

    /// Waits for old readers and returns the payload to the preparing task.
    ///
    /// An unmatched withdrawal returns None without submitting a marker or
    /// waiting. A match frees only node storage here; the caller owns T and
    /// chooses where its destructor runs. This is not a callback-drain barrier
    /// or proof that a device producer or independently owned operation stopped.
    ///
    /// # Safety
    /// Restore the preparing task's watched, sleepable context: no read scope,
    /// IRQ/preemption exclusion or lock needed by readers/RCU progress may remain.
    /// Keep the permanent RCU service and participants available until return.
    pub unsafe fn reclaim(self) -> Option<T> {
        if self.raw.is_null() {
            return None;
        }
        // SAFETY: the caller restores the preparing task's context. This wait
        // is submitted strictly after unlink, covering every old scoped borrow.
        unsafe { self.wait.submit_and_wait() };
        // SAFETY: withdrawal transferred this allocation's unique retirement
        // right; the completed GP ended old borrows. Its callback was not queued.
        let node = unsafe { KBox::from_raw(self.raw) };
        Some(KBox::into_inner(node).value)
    }
}

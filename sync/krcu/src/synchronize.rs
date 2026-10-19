// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Task-bound synchronous waits; callback completion never destroys retired data.

use alloc::sync::Arc;
use core::sync::atomic::{Ordering, fence};

use kerrno::{KError, KResult};
use kpoll::{Completion, PollRegisterError, PollRegistration};
use krcu_core::CallbackNode;
use ktask::future::PreparedTaskWait;

use super::{GracePeriodTask, SERVICE};

#[repr(C)]
struct GracePeriodCompletion {
    node: CallbackNode,
    completion: Completion,
}

// The callback recovers the Arc allocation directly from its node pointer.
const _: () = assert!(core::mem::offset_of!(GracePeriodCompletion, node) == 0);

/// Prepared before withdrawal so no recoverable setup failure can abandon it.
pub(super) struct PreparedGracePeriodWait {
    service: &'static GracePeriodTask,
    completion: Arc<GracePeriodCompletion>,
    wait: PreparedTaskWait,
    _registration: PollRegistration,
}

impl PreparedGracePeriodWait {
    pub(super) fn prepare() -> KResult<Self> {
        let wait = PreparedTaskWait::prepare()?;
        let service = SERVICE
            .get()
            .filter(|service| service.waker.get().is_some())
            .ok_or(KError::BadState)?;
        let completion = Arc::new(GracePeriodCompletion {
            node: CallbackNode::new(complete_grace_period),
            completion: Completion::new(),
        });
        let registration = completion
            .completion
            .register_owned(&wait.waker())
            .map_err(|error| match error {
                PollRegisterError::NoMemory | PollRegisterError::IdExhausted => KError::NoMemory,
                PollRegisterError::InvalidState => KError::InvalidInput,
            })?;
        Ok(Self {
            service,
            completion,
            wait,
            _registration: registration,
        })
    }

    pub(super) fn submit_and_wait(mut self) {
        let record = Arc::into_raw(self.completion.clone());
        // SAFETY: projection retains the original Arc allocation provenance;
        // only the node's interior-mutable linkage is modified by the queue.
        let node = unsafe { &raw const (*record).node }.cast_mut();
        {
            let _pin = kspin::NoPreempt::new();
            let cpu = khal::percpu::this_cpu_id().as_usize();
            // SAFETY: preparation verified the permanent boot service. Every
            // executing CPU belongs to its fixed participant set. This fresh
            // node has one queue owner; the raw Arc remains owned by its
            // nonblocking callback, independently of the waiter's Arc.
            let accepted = unsafe { self.service.callbacks.submit(cpu, node) };
            // Startup publishes the waker only after claiming the permanent
            // driver, and every running CPU belongs to its fixed participant
            // set. Rejection breaks that boot invariant and is fatal: an owning
            // replacement may already have withdrawn its old allocation, so
            // this is not a recoverable setup error after publication changed.
            assert!(accepted, "prepared RCU wait requires a participating CPU");
        }
        // Raising the notification may enter the scheduler without IPIs.
        // Leave queue locks and the submission pin before requesting progress.
        self.service.request_pass();
        while !self.completion.completion.try_wait() {
            self.wait.park();
        }
        // Carry the RCU completion ordering to subsequent caller accesses even
        // when completion preceded park or the waiter resumed on another CPU.
        fence(Ordering::SeqCst);
    }
}

/// # Safety
/// `node` must be the provenance-preserving field projection of `node` from a
/// `GracePeriodCompletion` pointer returned by `Arc::into_raw`. Its zero offset
/// is checked at compile time. Invoke exactly once after the GP, consuming that
/// transferred strong reference, outside RCU locks and in a context permitting
/// completion wakeups and Arc cleanup.
unsafe fn complete_grace_period(node: *mut CallbackNode) {
    // SAFETY: repr(C) places node first; submission retained this original Arc
    // reference. The waiter owns a separate reference and may already return
    // while complete() is still notifying it on another CPU.
    let record = unsafe { Arc::from_raw(node.cast::<GracePeriodCompletion>()) };
    record.completion.complete();
}

/// Waits for a future grace period through the boot-owned callback service.
///
/// All fallible preparation precedes submission. Once submitted, signals and
/// spurious wakes cannot end the wait early. No retired object is destroyed by
/// this operation; the caller retains responsibility for its ownership.
/// Allocation uses the kernel allocator's fatal OOM policy, except fallible
/// completion registration. This is not a callback barrier.
///
/// # Errors
/// Returns `InvalidInput` outside sleepable task context (including a local
/// read scope), `BadState` before service readiness, or `NoMemory` if completion
/// registration cannot be stored. No callback is submitted on error.
///
/// # Safety
/// Limited to controlled task-reader integration: every protected access must
/// use its current task's scoped reader in watched kernel execution. IRQ/NMI,
/// idle and user-context reader integration is not supplied by this interface.
/// Withdraw every relevant publication route before calling, and retain the
/// old object until successful return. Hold no lock needed by a reader or the
/// service. An error does not authorize reclamation. The boot-owned service
/// and participating CPUs must remain available throughout the wait.
unsafe fn synchronize_rcu() -> KResult<()> {
    PreparedGracePeriodWait::prepare()?.submit_and_wait();
    Ok(())
}

unittest::path_mod_test!(tests, "../ut-tests/rcu_synchronize.rs");

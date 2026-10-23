// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-request waiting and per-device completion contracts.

use alloc::{boxed::Box, sync::Arc};
use core::marker::PhantomData;

use kspin::SpinNoIrq;

use crate::DriverResult;

/// Driver operations for reclaiming a block device's completed I/O requests.
///
/// Implemented by the device that owns the hardware request queue, not by a
/// separate callback object. The host decides when to invoke these operations;
/// the driver identifies completed requests, publishes results and notifies
/// their callers. This interface owns no host dispatch state.
///
/// Each device has one host completion-processing state. Its activation owner
/// retains a strong reference until completion processing has stopped.
pub trait BlockCompletionOperations: Send + Sync {
    /// Reclaims a finite batch of completed requests and notifies their callers.
    ///
    /// May run with IRQs enabled in non-sleepable context. Release device locks
    /// before notifying callers. Replenishment may submit already-owned pending
    /// requests through nonblocking transport operations. Do not wait or grow software
    /// queue bookkeeping here; DMA operations must support this execution context.
    /// The host serializes invocations for the same device through its single
    /// completion-processing state, including the notification part of a pass.
    fn process_completed_requests(&self);
}

/// Independently owned notification sources for one block transaction.
///
/// Methods must be non-sleeping and IRQ-safe. Invoke them after releasing
/// device and registry locks. Implementations must not retain request, data
/// buffer or device pointers; a signal may outlive the originating call.
pub trait BlockSignals: Send + Sync {
    /// Permanently signals terminal completion.
    ///
    /// Publish the request result and relinquish device buffer ownership first.
    /// Repeated notification must not reset completion or create a new result.
    fn notify_completion(&self);
}

/// Task-local waiting half of a block transaction's completion state.
///
/// Only the preparing task may call these methods, without locks that forbid
/// sleeping. The waiter stays outside the shared request node. Notification
/// does not itself own the driver's request state or result.
pub trait BlockWaiter {
    /// Returns a shareable handle to this waiter's existing notification state.
    ///
    /// Cloning the handle must not allocate or create another notification
    /// source. It may outlive this task-bound waiter.
    fn signals(&self) -> Arc<dyn BlockSignals>;

    /// Waits non-interruptibly for the permanent terminal notification.
    ///
    /// May establish the task wait lazily when completion is still pending,
    /// but must not return a wait error after device ownership has started.
    /// Spurious task wakes must not let this method return before completion.
    /// Infallible wait allocation follows the host kernel's OOM policy.
    fn wait_completion(&mut self);
}

/// Host task-context checks and waiting resources for synchronous bio callers.
///
/// Checking context precedes submission and must not allocate. Waiting resources
/// are needed only when a caller observes an incomplete bio. Notification and
/// result publication remain independent of whether that caller still exists.
pub trait BlockWaitProvider: Sync {
    /// Checks that the caller is in IRQ-enabled, sleepable task context.
    ///
    /// # Errors
    /// Returns InvalidInput for an invalid context, before device ownership starts.
    fn validate_context(&self) -> DriverResult;

    /// Prepares a waiter in a context accepted by [`Self::validate_context`].
    ///
    /// This runs after submission and must not fail recoverably: the caller
    /// cannot abandon borrowed memory before device completion. Allocation
    /// follows the host's infallible OOM policy.
    fn prepare_wait(&self) -> Box<dyn BlockWaiter>;
}

static WAIT_PROVIDER: klazy::Once<&'static dyn BlockWaitProvider> = klazy::Once::new();

/// Installs the host waiting provider before block I/O consumers are started.
pub fn init_wait_provider(provider: &'static dyn BlockWaitProvider) {
    WAIT_PROVIDER.call_once(|| provider);
}

/// Terminal status and original memory owners, returned independently on every outcome.
pub type BioResult = (DriverResult, alloc::vec::Vec<crate::BioVec>);

pub(crate) struct BioWaitState {
    result: Option<BioResult>,
    notification: Option<Arc<dyn BlockSignals>>,
}

impl BioWaitState {
    pub(crate) fn complete(state: &SpinNoIrq<Self>, result: BioResult) {
        let notification = {
            let mut state = state.lock();
            state.result = Some(result);
            state.notification.take()
        };
        // Notification may wake a task immediately; never invoke it under the
        // result lock. The bio retains the state through this final access.
        if let Some(notification) = notification {
            notification.notify_completion();
        }
    }
}

/// Task-bound synchronous adapter; notification reuses [`BlockWaiter`].
/// Dropping this handle never cancels or frees an in-flight bio's memory.
pub struct BioWait {
    state: Arc<SpinNoIrq<BioWaitState>>,
    _task_bound: PhantomData<*mut ()>,
}

impl BioWait {
    /// Prepares a bio and its independent result owner in sleepable task context.
    /// Several bios can be submitted before any of their handles are waited on.
    /// No host waiting resources are allocated until an incomplete bio is waited on.
    ///
    /// # Errors
    /// Returns BadState before host setup, or the provider's context error,
    /// together with every original memory owner. No I/O has been submitted.
    pub fn prepare(
        device: Arc<crate::BlockDevice>,
        sector: u64,
        operation: crate::BioOperation,
        vectors: alloc::vec::Vec<crate::BioVec>,
    ) -> Result<(crate::Bio, Self), (crate::DriverError, alloc::vec::Vec<crate::BioVec>)> {
        if let Err(error) = WAIT_PROVIDER
            .get()
            .ok_or(crate::DriverError::BadState)
            .and_then(|provider| provider.validate_context())
        {
            return Err((error, vectors));
        }
        let state = Arc::new(SpinNoIrq::new(BioWaitState {
            result: None,
            notification: None,
        }));
        let bio = crate::Bio::with_completion(
            device,
            sector,
            operation,
            vectors,
            crate::bio::BioEndIo::Wait(state.clone()),
        );
        Ok((
            bio,
            Self {
                state,
                _task_bound: PhantomData,
            },
        ))
    }

    /// Waits for terminal completion and returns status alongside the original
    /// memory, including validation and device failures. Reads may be partial.
    /// Call from the preparing task without locks that forbid sleeping.
    ///
    /// # Panics
    /// Panics outside sleepable task context.
    pub fn wait(self) -> BioResult {
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: prepare checked the once-installed \
                      provider; BioWait is task-bound and wait requires sleepable context"
        )]
        let provider = {
            let provider = WAIT_PROVIDER
                .get()
                .expect("provider checked before submission");
            provider.validate_context().expect("bio wait task context");
            provider
        };
        self.wait_with(|| provider.prepare_wait())
    }

    fn wait_with(self, prepare: impl FnOnce() -> Box<dyn BlockWaiter>) -> BioResult {
        if let Some(result) = self.state.lock().result.take() {
            return result;
        }
        let mut waiter = prepare();
        let notification = waiter.signals();
        {
            let mut state = self.state.lock();
            // Completion may race with waiter allocation. Otherwise publish
            // notification under the same lock used to publish the result.
            if let Some(result) = state.result.take() {
                return result;
            }
            state.notification = Some(notification);
        }
        waiter.wait_completion();
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: BioWaitState::complete publishes the \
                      result under the result lock before notifying the enrolled signal; \
                      wait_completion returns only after that signal"
        )]
        self.state
            .lock()
            .result
            .take()
            .expect("terminal bio result published before notification")
    }
}

unittest::path_mod_test!(tests, "../ut-tests/completion.rs");

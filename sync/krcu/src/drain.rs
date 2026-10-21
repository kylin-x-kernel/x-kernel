// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Serialized callback drains with reusable markers and independent wake storage.

use alloc::sync::Arc;
use core::{
    sync::atomic::{AtomicUsize, Ordering},
    task::Waker,
};

use kerrno::{KError, KResult};
use krcu_core::CallbackNode;
use kspin::SpinNoIrq;
use ksync::Mutex;
use ktask::future::PreparedTaskWait;

use super::SERVICE;

/// Permanent marker storage is separate from the mutex's exclusively borrowed
/// payload: callbacks access markers while the draining task holds that mutex.
pub(super) struct CallbackDrain {
    serialization: Mutex<()>,
    markers: [DrainMarker; kbuild_config::NR_CPUS],
}

#[repr(C)]
struct DrainMarker {
    node: CallbackNode,
    completion: SpinNoIrq<Option<Arc<DrainCompletion>>>,
}

const _: () = assert!(core::mem::offset_of!(DrainMarker, node) == 0);

struct DrainCompletion {
    remaining: AtomicUsize,
    waker: Waker,
}

impl CallbackDrain {
    pub(super) fn new() -> Self {
        Self {
            serialization: Mutex::new(()),
            markers: core::array::from_fn(|_| DrainMarker {
                node: CallbackNode::new(complete_marker),
                completion: SpinNoIrq::new(None),
            }),
        }
    }
}

impl DrainCompletion {
    fn new(waker: Waker) -> Self {
        Self {
            // Installation owns one reference even if earlier markers run
            // before the draining task reaches the next CPU's queue.
            remaining: AtomicUsize::new(1),
            waker,
        }
    }

    fn finish_one(&self) {
        // Every participant releases its preceding callback effects. The final
        // RMW acquires the chain before notifying the acquire-loading waiter.
        if self.remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.waker.wake_by_ref();
        }
    }

    fn is_complete(&self) -> bool {
        self.remaining.load(Ordering::Acquire) == 0
    }
}

/// Waits for callbacks accepted before this call to return, not merely for a GP.
///
/// Concurrent drains serialize sleepably. Later submissions/reposts may be
/// included but are not chased; separately queued downstream work is excluded.
/// Teardown must stop producers and reposts first. Signals and spurious wakes
/// cannot shorten an installed drain. Empty queues require no new GP.
///
/// # Errors
/// Returns `InvalidInput` outside sleepable watched task context (including a
/// read scope), or `BadState` before service readiness, before publishing any
/// marker. Wait and completion allocations follow fatal kernel OOM policy.
///
/// # Safety
/// Hold no resource required by a callback or the GP service. The permanent
/// service and fixed participating CPU services must remain available. Callback
/// code and referenced resources must stay live until successful return. An
/// error does not authorize releasing them, and downstream work needs its own
/// completion protocol even after this function succeeds.
pub unsafe fn drain_callbacks() -> KResult<()> {
    let mut wait = PreparedTaskWait::prepare()?;
    {
        let _guard = kspin::NoPreemptIrqSave::new();
        if !krcu_core::RcuContextIf::current_cpu_watching().is_watching() {
            return Err(KError::InvalidInput);
        }
    }
    let service = SERVICE
        .get()
        .filter(|service| service.waker.get().is_some())
        .ok_or(KError::BadState)?;
    let completion = Arc::new(DrainCompletion::new(wait.waker()));
    let _serial = service.drain.serialization.lock();
    let mut has_markers = false;
    for cpu in 0..kcpu_id_map::nr_cpus() {
        let marker = &service.drain.markers[cpu];
        completion.remaining.fetch_add(1, Ordering::Relaxed);
        *marker.completion.lock() = Some(completion.clone());
        let record = core::ptr::from_ref(marker);
        // SAFETY: project from the whole permanent marker, preserving container
        // provenance for the callback. Interior mutation belongs to its locks.
        let node = unsafe { &raw const (*record).node }.cast_mut();
        // SAFETY: the mutex serializes installations. The preceding drain ended
        // all marker accesses before publishing completion, and the executor
        // never touches a node after invocation. This marker owns no retired
        // data and only completes after prior callbacks return. Queue locks
        // serialize remote insertion with capture/execution; no host lock that
        // forbids sleeping is held across progress notification or waiting.
        if unsafe { service.callbacks.enqueue_drain_marker(cpu, node) } {
            has_markers = true;
        } else {
            // Empty/nonparticipating queues do not take ownership. Roll back
            // their count while the installation reference still prevents zero.
            marker.completion.lock().take();
            completion.finish_one();
        }
    }
    if has_markers {
        service.request_pass();
    }
    completion.finish_one();
    while !completion.is_complete() {
        wait.park();
    }
    Ok(())
}

/// # Safety
/// `node` is the zero-offset, provenance-preserving projection of one permanent
/// DrainMarker, installed exactly once under its drain serialization mutex.
/// The executor has unlinked it and will never access it after invocation.
/// Invoke outside queue locks in a context permitting nonblocking wakeups.
unsafe fn complete_marker(node: *mut CallbackNode) {
    let completion = {
        // SAFETY: repr(C) and the offset assertion recover the original stable
        // marker. Its completion slot has independent interior synchronization.
        let marker = unsafe { &*node.cast::<DrainMarker>() };
        marker.completion.lock().take()
    };
    // No marker access or borrow survives this point. The waiter may return and
    // reuse its node as soon as finish_one publishes zero. Only this independent
    // Arc is used while wakeup unwinds, never the next drain's completion state.
    if let Some(completion) = completion {
        completion.finish_one();
    }
}

unittest::path_mod_test!(tests, "../ut-tests/rcu_drain.rs");

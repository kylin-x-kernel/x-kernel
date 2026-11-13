// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Deterministic interleaving points used by unit tests.
//!
//! These seams exist only in `cfg(unittest)` builds. A callback may block (for
//! example on a test `ksync` semaphore) because every pause point is outside
//! registry/bus spinlocks. The probe, adoption, and remove points are inside
//! the relevant binding critical section; the registration-scan point fires
//! just before that scan takes the lock.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use kspin::SpinNoPreempt;
use lazyinit::LazyInit;

use crate::{DeviceDescId, DeviceId};

/// A point where a test can deterministically pause a driver-core path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingPause {
    /// A driver registration scan is about to wait for this descriptor's
    /// binding lock.
    RegisterScanBeforeLock { desc_id: DeviceDescId },
    /// Registration took the descriptor's fresh binding lock and is about to
    /// publish the object/descriptor and perform the initial topology link.
    ///
    /// The callback runs with the binding lock held, outside every spinlock,
    /// so it may block.
    RegistrationHoldingLock { desc_id: DeviceDescId },
    /// `probe_device_desc` acquired the binding lock and is about to
    /// re-read the descriptor state.
    ProbeHoldingLock { desc_id: DeviceDescId },
    /// Adoption published the device and holds its binding lock, before
    /// `Activated` is dispatched.
    AdoptionBeforeActivate { device_id: DeviceId },
    /// `unregister_device_desc` acquired the binding lock and is about to
    /// re-read the descriptor state.
    UnregisterHoldingLock { desc_id: DeviceDescId },
    /// Remove applied the descriptor disposition (requeued it, or deleted the
    /// registration) but has not torn down the old bus/driver relations yet.
    RemoveAfterRequeue {
        desc_id: DeviceDescId,
        device_id: DeviceId,
    },
    /// A binding CASed its state to `Removing` and is about to check for
    /// outstanding [`crate::DeviceUse`] guards.
    ///
    /// The state is transient here: a busy removal restores the prior state
    /// after this point. The callback runs in the removal task context, outside
    /// every spinlock, so it may block.
    BindingRemovingBeforeUseCheck,
    /// Registration took the parent/child topology snapshot and is about to
    /// acquire the ordered relations locks.
    ///
    /// The callback runs in the registration task context (holding the child's
    /// descriptor binding lock) without holding any relations lock, so it may
    /// block.
    TopologyBeforeLink {
        parent_id: DeviceId,
        child_id: DeviceId,
    },
    /// The relation was written and the topology layer is about to recheck
    /// membership before reporting success.
    TopologyAfterLink {
        parent_id: DeviceId,
        child_id: DeviceId,
    },
    /// Unregister ended the registration and is about to conditionally clear
    /// the snapshot of topology relations.
    UnregisterBeforeTopologyCleanup { device_id: DeviceId },
    /// A new device node was published and is about to dispatch its add
    /// callbacks.
    NodePublishing { device_id: DeviceId },
    /// A device node became unavailable and is about to dispatch its removal
    /// callbacks (and, for class-owned nodes, before the class is unpublished).
    NodeRemoving { device_id: DeviceId },
}

type RelationsLockedCallback = Arc<dyn Fn(&[DeviceId]) + Send + Sync>;

static RELATIONS_LOCKED: LazyInit<SpinNoPreempt<Option<RelationsLockedCallback>>> = LazyInit::new();

fn relations_slot() -> &'static SpinNoPreempt<Option<RelationsLockedCallback>> {
    RELATIONS_LOCKED.call_once(|| SpinNoPreempt::new(None));
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: call_once permanently published the \
                  relations-lock slot"
    )]
    RELATIONS_LOCKED
        .get()
        .expect("relations slot call_once succeeded")
}

/// Install a callback invoked while a caller holds every relation lock in
/// `ids` order.
///
/// The callback runs with **preemption disabled** and with the relation spin
/// locks held. It must never sleep, block, allocate a blocking gate, or call
/// back into kdevice. Use it only for a bounded atomic handshake; tests that
/// need to pause a task must use [`install_pause_hook`] instead.
pub fn install_relations_lock_hook(callback: RelationsLockedCallback) {
    *relations_slot().lock() = Some(callback);
}

/// Remove a previously installed relations-lock callback.
pub fn clear_relations_lock_hook() {
    *relations_slot().lock() = None;
}

pub(crate) fn relations_locked(ids: &[DeviceId]) {
    // Clone under the slot lock, then release it before invoking the callback:
    // the callback must not observe the slot lock and must be allowed to spin.
    let callback = relations_slot().lock().clone();
    if let Some(callback) = callback {
        callback(ids);
    }
}

/// Whether the current task holds the registration's binding lock.
///
/// Test-only probe used to prove the fresh descriptor lock is held through
/// publication and the initial topology link without attempting to acquire it.
pub fn binding_is_owned_by_current(desc_id: DeviceDescId) -> bool {
    crate::device_registry()
        .binding_for_desc(desc_id)
        .is_some_and(|binding| binding.is_owned_by_current())
}

/// Run `action` while holding `device_id`'s descriptor binding lock.
///
/// Test-only helper for scenarios that must publish or mutate a class device
/// after an adoption has returned, and therefore need to re-enter the same
/// lifecycle write context the probe/adoption path provides.
pub fn with_binding_lock<R>(
    device_id: DeviceId,
    action: impl FnOnce() -> R,
) -> Result<R, crate::DriverError> {
    let binding = super::binding_for_device(device_id)?;
    let _guard = binding.lock();
    Ok(action())
}

type PauseCallback = Arc<dyn Fn(BindingPause) + Send + Sync>;

static PAUSE: LazyInit<SpinNoPreempt<Option<PauseCallback>>> = LazyInit::new();
/// Number of successful candidate setups before the injected failure, or
/// `usize::MAX` when no failure is armed.
static FAIL_CANDIDATE_SETUP: AtomicUsize = AtomicUsize::new(usize::MAX);

fn pause_slot() -> &'static SpinNoPreempt<Option<PauseCallback>> {
    PAUSE.call_once(|| SpinNoPreempt::new(None));
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: call_once permanently published the pause \
                  slot"
    )]
    PAUSE.get().expect("pause slot call_once succeeded")
}

/// Install the callback invoked at every subsequent pause point.
pub fn install_pause_hook(callback: PauseCallback) {
    *pause_slot().lock() = Some(callback);
}

/// Remove a previously installed pause callback.
pub fn clear_pause_hook() {
    *pause_slot().lock() = None;
}

pub(crate) fn pause(point: BindingPause) {
    // Clone the handle and release the slot lock before invoking the
    // callback: tests are allowed to block inside it.
    let callback = pause_slot().lock().clone();
    if let Some(callback) = callback {
        callback(point);
    }
}

/// Make the next candidate setup fail with `NoMemory`.
///
/// Candidate setup is the per-candidate step after the stable object exists and
/// before its fresh binding runtime is installed.
pub fn fail_next_candidate_setup() {
    fail_candidate_setup_after(0);
}

/// Allow `successful` candidate setups, then fail the one after them.
pub fn fail_candidate_setup_after(successful: usize) {
    assert!(
        successful < usize::MAX,
        "candidate setup failure countdown overflow"
    );
    FAIL_CANDIDATE_SETUP.store(successful, Ordering::SeqCst);
}

/// Consume one tick of the armed failure countdown.
///
/// Returns `true` exactly once when the countdown reaches zero, then
/// disarms so a retry can set up candidates again.
pub(crate) fn take_candidate_setup_failure() -> bool {
    let mut current = FAIL_CANDIDATE_SETUP.load(Ordering::SeqCst);
    loop {
        if current == usize::MAX {
            return false;
        }
        if current == 0 {
            match FAIL_CANDIDATE_SETUP.compare_exchange(
                0,
                usize::MAX,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return true,
                Err(observed) => {
                    current = observed;
                    continue;
                }
            }
        }
        match FAIL_CANDIDATE_SETUP.compare_exchange(
            current,
            current - 1,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) => return false,
            Err(observed) => current = observed,
        }
    }
}

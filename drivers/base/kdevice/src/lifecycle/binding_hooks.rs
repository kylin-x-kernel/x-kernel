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
    /// `probe_device_desc` acquired the binding lock and is about to
    /// re-read the descriptor state.
    ProbeHoldingLock { desc_id: DeviceDescId },
    /// Adoption published the device and holds its binding lock, before
    /// `Activated` is dispatched.
    AdoptionBeforeActivate { device_id: DeviceId },
    /// Remove restored the descriptor to `Pending` but has not torn down
    /// the old bus/driver relations yet.
    RemoveAfterRequeue {
        desc_id: DeviceDescId,
        device_id: DeviceId,
    },
}

type PauseCallback = Arc<dyn Fn(BindingPause) + Send + Sync>;

static PAUSE: LazyInit<SpinNoPreempt<Option<PauseCallback>>> = LazyInit::new();
/// Number of successful constructions before the injected failure, or
/// `usize::MAX` when no failure is armed.
static FAIL_DEVICE_CONSTRUCTION: AtomicUsize = AtomicUsize::new(usize::MAX);

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

/// Make the next unpublished-device construction fail with `NoMemory`.
pub fn fail_next_device_construction() {
    fail_device_construction_after(0);
}

/// Allow `successful` constructions, then fail the one after them.
pub fn fail_device_construction_after(successful: usize) {
    assert!(
        successful < usize::MAX,
        "construction failure countdown overflow"
    );
    FAIL_DEVICE_CONSTRUCTION.store(successful, Ordering::SeqCst);
}

/// Consume one tick of the armed failure countdown.
///
/// Returns `true` exactly once when the countdown reaches zero, then
/// disarms so a retry can construct devices again.
pub(crate) fn take_device_construction_failure() -> bool {
    let mut current = FAIL_DEVICE_CONSTRUCTION.load(Ordering::SeqCst);
    loop {
        if current == usize::MAX {
            return false;
        }
        if current == 0 {
            match FAIL_DEVICE_CONSTRUCTION.compare_exchange(
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
        match FAIL_DEVICE_CONSTRUCTION.compare_exchange(
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

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Deterministic class-retirement pause points used by unit tests.
//!
//! A callback may block: every pause point runs outside the class registry
//! spinlock and outside the device-node registry lock.

use alloc::sync::Arc;

use kdevice::DeviceId;
use kspin::SpinNoPreempt;
use lazyinit::LazyInit;

/// A point where a test can pause the class-retirement path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassPause {
    /// A caller claimed the same-ID retirement reservation and is about to
    /// unpublish the removed class entries.
    RetirementClaimed { id: DeviceId },
    /// Every removed entry has run `unpublish_class`; the same-ID reservation
    /// is still installed until `finish_retirement`.
    RetirementUnpublished { id: DeviceId },
}

type PauseCallback = Arc<dyn Fn(ClassPause) + Send + Sync>;

static PAUSE: LazyInit<SpinNoPreempt<Option<PauseCallback>>> = LazyInit::new();

fn pause_slot() -> &'static SpinNoPreempt<Option<PauseCallback>> {
    PAUSE.call_once(|| SpinNoPreempt::new(None));
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: call_once permanently published the pause \
                  slot"
    )]
    PAUSE.get().expect("pause slot call_once succeeded")
}

/// Install the callback invoked at every subsequent class pause point.
pub fn install_pause_hook(callback: PauseCallback) {
    *pause_slot().lock() = Some(callback);
}

/// Remove a previously installed pause callback.
pub fn clear_pause_hook() {
    *pause_slot().lock() = None;
}

pub(crate) fn pause(point: ClassPause) {
    // Clone the handle and release the slot lock before invoking the
    // callback: tests are allowed to block inside it.
    let callback = pause_slot().lock().clone();
    if let Some(callback) = callback {
        callback(point);
    }
}

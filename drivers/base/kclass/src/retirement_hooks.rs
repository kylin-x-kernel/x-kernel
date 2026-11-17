// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Deterministic publication activation/revocation pause points used by unit tests.
//!
//! A callback may block: every pause point runs outside the class registry
//! spinlock and outside the device-node registry lock.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use kdevice::{DeviceId, PublicationId};
use kspin::SpinNoPreempt;
use lazyinit::LazyInit;

/// A point where a test can pause publication activation/revocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClassPause {
    /// An activation executor claimed one publication.
    ActivationClaimed {
        id: DeviceId,
        publication: PublicationId,
    },
    /// Before a publication's node is registered.
    BeforeNodeRegistration {
        id: DeviceId,
        publication: PublicationId,
    },
    /// Activating is about to commit available callbacks (Notifying).
    BeforeCallbackCommit {
        id: DeviceId,
        publication: PublicationId,
    },
    /// Callbacks are committed, before the first callback runs.
    AfterCallbackCommit {
        id: DeviceId,
        publication: PublicationId,
    },
    /// Callbacks finished, before Notifying becomes Available.
    BeforeAvailableFinalize {
        id: DeviceId,
        publication: PublicationId,
    },
    /// Before the class backend unpublishes one runtime.
    BeforeUnpublish {
        id: DeviceId,
        publication: PublicationId,
    },
    /// Before removed subscribers run.
    BeforeRemovedCallback {
        id: DeviceId,
        publication: PublicationId,
    },
    /// Before the terminal registry removal exposes Revoked.
    BeforeFinalRevoked {
        id: DeviceId,
        publication: PublicationId,
    },
}

type PauseCallback = Arc<dyn Fn(ClassPause) + Send + Sync>;

static PAUSE: LazyInit<SpinNoPreempt<Option<PauseCallback>>> = LazyInit::new();
static FORCE_NODE_NOT_OWNER: AtomicBool = AtomicBool::new(false);

/// Force the next revocation execute step to report `NodeRemoval::NotOwner`.
pub fn force_node_not_owner() {
    FORCE_NODE_NOT_OWNER.store(true, Ordering::SeqCst);
}

pub(crate) fn take_forced_node_not_owner() -> bool {
    FORCE_NODE_NOT_OWNER.swap(false, Ordering::SeqCst)
}

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

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Cancellation owns detached cleanup; later Work captures do not observe it.

use super::Work;

impl<T, H> Work<T, H> {
    /// Withdraws pending work and releases its resources outside Work protection.
    ///
    /// Previously attached observers follow the detached resources and complete
    /// only after destruction. Later captures see current slots, not this cleanup.
    /// No Work access occurs after detachment, including from completion code.
    /// This does not wait for running work or drain timer delivery. Resource
    /// destructors must obey the calling context and must not wait for a scope
    /// containing their own cleanup; such misuse need not be detected.
    ///
    /// # Panics
    /// Propagates resource destructor or completion Waker panics. Failed resource
    /// destruction does not publish successful observer completion.
    pub fn cancel_pending(&self) -> bool {
        self.cancel_pending_with(|| {})
    }

    /// Cancels with one internal nonwaiting provider action under Work protection.
    /// The action must not sleep, allocate, unwind, reenter Work or acquire a
    /// partition lock. It runs once after route validation, including idle Work,
    /// before detachment; delayed cancellation uses it to serialize timer removal
    /// with rearm. No action or Work access follows detached cleanup notification.
    ///
    /// # Panics
    /// Propagates invariant, resource destruction and completion notification failures.
    pub(super) fn cancel_pending_with(&self, before_detach: impl Fn()) -> bool {
        let detached = loop {
            let partition = self.state.lock().pending_partition();
            if let Some(partition) = partition {
                if let Some(detached) = partition.detach_pending(self, &before_detach) {
                    break Some(detached);
                }
                continue;
            }
            let mut state = self.state.lock();
            if state.has_partitioned_pending() {
                continue;
            }
            before_detach();
            break state.cancel_pending().map(|slot| (slot, None));
        };
        let Some((slot, kick)) = detached else {
            return false;
        };
        if let Some(kick) = kick {
            kick.wake_by_ref();
        }
        slot.complete();
        true
    }
}

unittest::path_mod_test!(
    cancellation_unittest,
    "../../ut-tests/work_cancellation_unittest.rs"
);

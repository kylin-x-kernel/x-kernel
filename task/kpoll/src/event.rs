// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::sync::atomic::{AtomicUsize, Ordering};

use crate::{PollContext, PollRegisterError, PollSet};

/// Broadcast poll event with generation-based lost-wakeup protection.
///
/// `PollEvent` is for state changes that do not carry completion tokens. Waiters
/// remember a generation, register on the broadcast source, then recheck the
/// generation before sleeping. A changed generation means the protected
/// predicate must be re-evaluated.
pub struct PollEvent {
    generation: AtomicUsize,
    waiters: PollSet,
}

impl PollEvent {
    /// Creates an event source with generation zero.
    pub fn new() -> Self {
        Self {
            generation: AtomicUsize::new(0),
            waiters: PollSet::new(),
        }
    }

    /// Returns the current event generation.
    pub fn generation(&self) -> usize {
        self.generation.load(Ordering::Acquire)
    }

    /// Returns whether this event has changed since `observed_generation`.
    pub fn has_changed_since(&self, observed_generation: usize) -> bool {
        self.generation() != observed_generation
    }

    /// Publishes one state change and wakes registered waiters.
    ///
    /// Returns the number of waiter registrations woken by the underlying
    /// [`PollSet`]. The count is diagnostic and test information; it does not
    /// mean a waiter consumed work or that the protected predicate became true.
    pub fn notify(&self) -> usize {
        self.notify_defer_wake().wake()
    }

    /// Publishes one state change and returns its wake source.
    ///
    /// This is for callers that need to update another lock-protected predicate
    /// and the event generation together, but wake waiters after releasing the
    /// outer lock. Callers should publish the protected state first, then call
    /// this method before dropping the outer lock; the returned [`PollSet`] must
    /// be woken only after that outer lock is released.
    pub fn notify_defer_wake(&self) -> PollSet {
        self.generation.fetch_add(1, Ordering::Release);
        self.waiters.clone()
    }

    /// Registers the current logical wait for this event source.
    ///
    /// Callers must recheck [`Self::has_changed_since`] after registration.
    pub fn register(&self, context: &mut PollContext<'_>) -> Result<(), PollRegisterError> {
        context.register(&self.waiters)
    }
}

impl Default for PollEvent {
    fn default() -> Self {
        Self::new()
    }
}

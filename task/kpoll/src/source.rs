// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{sync::Arc, task::Wake};
use core::{mem, task::Waker};

use kspin::SpinNoIrq;
use smallvec::SmallVec;

use crate::{PollRegisterError, registration::PollRegistration};

const INLINE_WAITER_SLOTS: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RegistrationToken {
    slot: usize,
    id: u64,
}

enum WaiterSlot {
    Occupied { id: u64, waker: Waker },
    Vacant { next: Option<usize> },
}

type WaiterSlots = SmallVec<[WaiterSlot; INLINE_WAITER_SLOTS]>;

struct State {
    slots: WaiterSlots,
    free_head: Option<usize>,
    active: usize,
    next_id: u64,

    #[cfg(feature = "stats")]
    register_count: usize,
    #[cfg(feature = "stats")]
    wake_count: usize,
    #[cfg(feature = "stats")]
    cancel_count: usize,
}

impl State {
    const fn new() -> Self {
        Self {
            slots: SmallVec::new_const(),
            free_head: None,
            active: 0,
            next_id: 1,

            #[cfg(feature = "stats")]
            register_count: 0,
            #[cfg(feature = "stats")]
            wake_count: 0,
            #[cfg(feature = "stats")]
            cancel_count: 0,
        }
    }

    fn register(&mut self, waker: Waker) -> Result<RegistrationToken, PollRegisterError> {
        // Reserve the next id without consuming it until the slot is installed,
        // so a failed `try_reserve` does not burn identity space.
        let next_id = self
            .next_id
            .checked_add(1)
            .ok_or(PollRegisterError::IdExhausted)?;
        let id = self.next_id;

        let slot = if let Some(slot) = self.free_head {
            let WaiterSlot::Vacant { next } = self.slots[slot] else {
                unreachable!("free-list entry must be vacant");
            };
            self.free_head = next;
            self.slots[slot] = WaiterSlot::Occupied { id, waker };
            slot
        } else {
            self.slots
                .try_reserve(1)
                .map_err(|_| PollRegisterError::NoMemory)?;
            let slot = self.slots.len();
            self.slots.push(WaiterSlot::Occupied { id, waker });
            slot
        };

        self.next_id = next_id;
        self.active += 1;
        #[cfg(feature = "stats")]
        {
            self.register_count = self.register_count.saturating_add(1);
        }
        Ok(RegistrationToken { slot, id })
    }

    fn unregister(&mut self, token: RegistrationToken) -> Option<Waker> {
        let entry = self.slots.get(token.slot)?;
        if !matches!(entry, WaiterSlot::Occupied { id, .. } if *id == token.id) {
            return None;
        }

        let WaiterSlot::Occupied { waker, .. } = mem::replace(
            &mut self.slots[token.slot],
            WaiterSlot::Vacant {
                next: self.free_head,
            },
        ) else {
            unreachable!("token matched an occupied slot");
        };
        self.free_head = Some(token.slot);
        self.active -= 1;
        #[cfg(feature = "stats")]
        {
            self.cancel_count = self.cancel_count.saturating_add(1);
        }
        Some(waker)
    }

    /// Detaches every waiter for an IRQ-safe wake.
    ///
    /// The returned slot table is traversed outside the source lock so the IRQ
    /// wake path does not allocate or invoke arbitrary waker code while holding
    /// `SpinNoIrq`.
    fn detach_slots(&mut self) -> WaiterSlots {
        let slots = mem::replace(&mut self.slots, SmallVec::new_const());
        self.free_head = None;
        self.active = 0;
        slots
    }
}

fn wake_detached_slots(slots: WaiterSlots) -> usize {
    let mut count = 0;
    for slot in slots {
        if let WaiterSlot::Occupied { waker, .. } = slot {
            count += 1;
            waker.wake();
        }
    }
    count
}

pub(crate) struct PollSetInner {
    state: SpinNoIrq<State>,
}

impl PollSetInner {
    fn new() -> Self {
        Self {
            state: SpinNoIrq::new(State::new()),
        }
    }

    pub(crate) fn unregister(&self, token: RegistrationToken) -> bool {
        // Drop the detached `Waker` only after releasing `SpinNoIrq`: custom
        // waker destructors must not re-enter while the source lock is held.
        let waker = self.state.lock().unregister(token);
        waker.is_some()
    }
}

impl Drop for PollSetInner {
    fn drop(&mut self) {
        wake_detached_slots(self.state.get_mut().detach_slots());
    }
}

/// A broadcast source for one-shot poll registrations.
///
/// Registrations are removed by [`Self::wake`] or when their owning
/// [`PollRegistration`] is dropped. `wake` may be called from IRQ context and
/// never invokes a waker while holding the internal spin lock.
pub struct PollSet {
    inner: Arc<PollSetInner>,
}

impl PollSet {
    /// Creates an empty poll source.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(PollSetInner::new()),
        }
    }

    /// Registers `waker` and returns an ownership guard for the registration.
    ///
    /// Prefer [`PollContext::register`] for ordinary poll waits. This method is
    /// intended for long-lived bridges that must retain the guard themselves.
    ///
    /// # Errors
    ///
    /// Returns an error when the waiter table cannot grow or registration
    /// identities are exhausted.
    pub fn register(&self, waker: &Waker) -> Result<PollRegistration, PollRegisterError> {
        // Clone before taking `SpinNoIrq`: `Waker::clone` may run arbitrary
        // code and must not re-enter while the source lock is held.
        let waker = waker.clone();
        let token = self.inner.state.lock().register(waker)?;
        Ok(PollRegistration::new(Arc::downgrade(&self.inner), token))
    }

    /// Wakes every currently registered waiter.
    ///
    /// A registration concurrently cancelled after this method detaches its
    /// waker may still receive one late wake. Callers must always recheck the
    /// readiness condition after being woken.
    pub fn wake(&self) -> usize {
        let slots = {
            let mut state = self.inner.state.lock();
            if state.active == 0 {
                return 0;
            }
            #[cfg(feature = "stats")]
            {
                let wake_count = state.active;
                state.wake_count = state.wake_count.saturating_add(wake_count);
            }
            // Single lock: detach the slot table. Wakers are invoked after
            // dropping `SpinNoIrq`, and no wake-path allocation is needed.
            state.detach_slots()
        };
        wake_detached_slots(slots)
    }

    #[cfg(feature = "stats")]
    /// Returns registration lifecycle statistics.
    pub fn stats(&self) -> WakerStats {
        let state = self.inner.state.lock();
        WakerStats {
            register_count: state.register_count,
            wake_count: state.wake_count,
            cancel_count: state.cancel_count,
            current_count: state.active,
        }
    }
}

impl Clone for PollSet {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl Default for PollSet {
    fn default() -> Self {
        Self::new()
    }
}

impl Wake for PollSet {
    fn wake(self: Arc<Self>) {
        self.as_ref().wake();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.as_ref().wake();
    }
}

#[cfg(feature = "stats")]
/// Statistics about a [`PollSet`]'s registration lifecycle.
#[derive(Debug, Clone, Copy)]
pub struct WakerStats {
    /// Total successful registrations.
    pub register_count: usize,
    /// Total registrations detached for wake-up.
    pub wake_count: usize,
    /// Total registrations removed by cancellation.
    pub cancel_count: usize,
    /// Current number of active registrations.
    pub current_count: usize,
}

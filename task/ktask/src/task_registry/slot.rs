// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Inline diagnostic storage with non-waiting retirement.
//!
//! T may be destroyed by the retiring caller or the final reader. Its Drop
//! must support those contexts; the registry uses the NMI-capable DeferredWeak.

use core::{
    cell::UnsafeCell,
    mem::MaybeUninit,
    sync::atomic::{AtomicUsize, Ordering},
};

const LIVE: usize = 1 << (usize::BITS - 1);
const RETIRED: usize = 1 << (usize::BITS - 2);
const READERS: usize = RETIRED - 1;

pub(super) struct Slot<T> {
    state: AtomicUsize,
    value: UnsafeCell<MaybeUninit<T>>,
}

// SAFETY: publication and replacement are exclusive state transitions. Readers
// hold a count over immutable T; retirement moves T out only after all counts
// end. T may be read and finally destroyed on a different CPU.
unsafe impl<T: Send + Sync> Sync for Slot<T> {}

impl<T> Slot<T> {
    pub(super) const fn new() -> Self {
        Self {
            state: AtomicUsize::new(0),
            value: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }

    pub(super) fn try_insert(&self, value: T) -> Result<(), T> {
        if self
            .state
            .compare_exchange(0, RETIRED, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return Err(value);
        }
        // SAFETY: claiming EMPTY excludes readers and other publishers; the
        // previous generation moved its value out before publishing EMPTY.
        unsafe { (*self.value.get()).write(value) };
        self.state.store(LIVE, Ordering::Release);
        Ok(())
    }

    #[cfg(any(
        target_arch = "aarch64",
        target_arch = "x86_64",
        feature = "watchdog",
        unittest,
        test
    ))]
    pub(super) fn read(&self) -> Option<Read<'_, T>> {
        let mut state = self.state.load(Ordering::Acquire);
        loop {
            if state & (LIVE | RETIRED) != LIVE || state & READERS == READERS {
                return None;
            }
            match self.state.compare_exchange_weak(
                state,
                state + 1,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Some(Read { slot: self }),
                Err(actual) => state = actual,
            }
        }
    }

    pub(super) fn retire(&self) {
        let mut state = self.state.load(Ordering::Acquire);
        loop {
            if state & (LIVE | RETIRED) != LIVE {
                return;
            }
            match self.state.compare_exchange_weak(
                state,
                state | RETIRED,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    if state & READERS == 0 {
                        // SAFETY: this CAS closes admission with no readers;
                        // only its winner owns removal for this generation.
                        unsafe { self.clear() };
                    }
                    return;
                }
                Err(actual) => state = actual,
            }
        }
    }

    /// # Safety
    /// Own the LIVE|RETIRED, zero-reader state for this generation exclusively.
    unsafe fn clear(&self) {
        // SAFETY: admission is closed and every counted borrow has ended.
        let storage = unsafe { &*self.value.get() };
        // SAFETY: LIVE certifies initialization and this generation's unique
        // remover moves the value before making the slot available again.
        let value = unsafe { storage.assume_init_read() };
        self.state.store(0, Ordering::Release);
        drop(value);
    }
}

impl<T> Drop for Slot<T> {
    fn drop(&mut self) {
        if *self.state.get_mut() & LIVE != 0 {
            // SAFETY: exclusive self excludes all usable Read guards. LIVE
            // means initialized; clear removes LIVE before dropping its value.
            unsafe { self.value.get_mut().assume_init_drop() };
        }
    }
}

#[cfg(any(
    target_arch = "aarch64",
    target_arch = "x86_64",
    feature = "watchdog",
    unittest,
    test
))]
pub(super) struct Read<'a, T> {
    slot: &'a Slot<T>,
}

#[cfg(any(
    target_arch = "aarch64",
    target_arch = "x86_64",
    feature = "watchdog",
    unittest,
    test
))]
impl<T> core::ops::Deref for Read<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: our count prevents any replacement of the slot's storage.
        let storage = unsafe { &*self.slot.value.get() };
        // SAFETY: successful Acquire admission observes initialization; our
        // counted guard covers this immutable borrow until it is dropped.
        unsafe { storage.assume_init_ref() }
    }
}

#[cfg(any(
    target_arch = "aarch64",
    target_arch = "x86_64",
    feature = "watchdog",
    unittest,
    test
))]
impl<T> Drop for Read<'_, T> {
    fn drop(&mut self) {
        let previous = self.slot.state.fetch_sub(1, Ordering::AcqRel);
        if previous == LIVE | RETIRED | 1 {
            // SAFETY: this is the last counted reader of a retired generation;
            // admission is closed and retirement left removal to that reader.
            unsafe { self.slot.clear() };
        }
    }
}

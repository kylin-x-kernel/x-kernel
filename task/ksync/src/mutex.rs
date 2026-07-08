// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! A blocking mutex implementation.

use core::sync::atomic::{AtomicU64, Ordering};

use event_listener::{Event, listener};
use ktask::{current, future::block_on};

use crate::util::{Spin, SpinConfig};

/// A [`lock_api::RawMutex`] implementation.
///
/// When the mutex is locked, the current task will block and be put into the
/// wait queue. When the mutex is unlocked, all tasks waiting on the queue
/// will be woken up.
pub struct RawMutex {
    event: Event,
    owner_id: AtomicU64,
    config: SpinConfig,
    #[cfg(feature = "stats")]
    stats: &'static klockstat::LockClassStats,
}

impl RawMutex {
    /// Creates a [`RawMutex`] with default spin configuration.
    #[inline(always)]
    pub const fn new() -> Self {
        Self::with_config(SpinConfig {
            max_spins: 10,
            spin_before_yield: 3,
        })
    }

    /// Creates a [`RawMutex`] with custom spin configuration.
    #[inline(always)]
    pub const fn with_config(config: SpinConfig) -> Self {
        Self {
            event: Event::new(),
            owner_id: AtomicU64::new(0),
            config,
            #[cfg(feature = "stats")]
            stats: &klockstat::NOOP_CLASS,
        }
    }
}

#[cfg(feature = "stats")]
impl RawMutex {
    /// Creates a [`RawMutex`] bound to `stats`.
    #[inline(always)]
    pub const fn new_with_stats(stats: &'static klockstat::LockClassStats) -> Self {
        Self {
            event: Event::new(),
            owner_id: AtomicU64::new(0),
            config: SpinConfig {
                max_spins: 10,
                spin_before_yield: 3,
            },
            stats,
        }
    }

    #[inline(always)]
    fn record_acquisitions(&self) {
        self.stats.record_acquisitions(1);
    }

    #[inline(always)]
    fn record_blocking(&self) {
        self.stats.record_contentions(1);
    }
}
impl Default for RawMutex {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: `RawMutex` provides the mutual-exclusion and wakeup guarantees
// required by `lock_api::RawMutex` through its internal state machine.
unsafe impl lock_api::RawMutex for RawMutex {
    type GuardMarker = lock_api::GuardSend;

    /// Initial value for an unlocked mutex.
    ///
    /// A “non-constant” const item is a legacy way to supply an initialized
    /// value to downstream static items. Can hopefully be replaced with
    /// `const fn new() -> Self` at some point.
    #[allow(clippy::declare_interior_mutable_const)]
    const INIT: Self = RawMutex::new();

    #[inline(always)]
    fn lock(&self) {
        #[cfg(feature = "stats")]
        self.stats.total_locks.fetch_add(1, Ordering::Relaxed);
        let current_id = current().owner_key();
        let mut spin = Spin::new(self.config);
        let mut owner_id = self.owner_id.load(Ordering::Relaxed);
        #[cfg(feature = "stats")]
        let mut did_block = false;

        loop {
            assert_ne!(
                owner_id,
                current_id,
                "{} tried to acquire mutex it already owns.",
                current().id_name()
            );

            if owner_id == 0 {
                match self.owner_id.compare_exchange_weak(
                    owner_id,
                    current_id,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        #[cfg(feature = "stats")]
                        self.record_acquisitions();

                        #[cfg(feature = "watchdog")]
                        {
                            current().inner().clear_waiting_lock();
                            current().inner().push_held_lock(self as *const _ as usize);
                        }
                        break;
                    }
                    Err(x) => owner_id = x,
                }
                continue;
            }

            if spin.spin() {
                owner_id = self.owner_id.load(Ordering::Relaxed);
                continue;
            }

            listener!(self.event => listener);

            owner_id = self.owner_id.load(Ordering::Acquire);
            if owner_id == 0 {
                continue;
            }

            #[cfg(feature = "stats")]
            if !did_block {
                self.record_blocking();
                did_block = true;
            }

            #[cfg(feature = "watchdog")]
            current()
                .inner()
                .set_waiting_lock(self as *const _ as usize, khal::time::now_ticks() as usize);
            block_on(listener);
            owner_id = self.owner_id.load(Ordering::Acquire);
        }
    }

    #[inline(always)]
    fn try_lock(&self) -> bool {
        let current_id = current().owner_key();
        // The reason for using a strong compare_exchange is explained here:
        // https://github.com/Amanieu/parking_lot/pull/207#issuecomment-575869107
        let acquired = self
            .owner_id
            .compare_exchange(0, current_id, Ordering::Acquire, Ordering::Relaxed)
            .is_ok();
        if acquired {
            #[cfg(feature = "stats")]
            self.record_acquisitions();
            #[cfg(feature = "watchdog")]
            current().inner().push_held_lock(self as *const _ as usize);
        }
        acquired
    }

    #[inline(always)]
    unsafe fn unlock(&self) {
        // SAFETY: `lock_api` only calls `unlock` after this mutex instance was
        // successfully locked by the current guard holder, so releasing the
        // owner slot and waking one waiter upholds the raw mutex protocol.
        let owner_id = self.owner_id.swap(0, Ordering::Release);
        assert_eq!(
            owner_id,
            current().owner_key(),
            "{} tried to release mutex it doesn't own",
            current().id_name()
        );
        #[cfg(feature = "watchdog")]
        current().inner().pop_held_lock(self as *const _ as usize);
        self.event.notify(1);
    }

    #[inline(always)]
    fn is_locked(&self) -> bool {
        self.owner_id.load(Ordering::Relaxed) != 0
    }
}

/// A kernel mutex built on [`lock_api::Mutex`].
pub struct Mutex<T>(lock_api::Mutex<RawMutex, T>);

impl<T> core::ops::Deref for Mutex<T> {
    type Target = lock_api::Mutex<RawMutex, T>;

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T: Default> Default for Mutex<T> {
    #[inline(always)]
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T: core::fmt::Debug> core::fmt::Debug for Mutex<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.0.fmt(f)
    }
}

impl<T> Mutex<T> {
    /// Creates a new [`Mutex`].
    #[inline(always)]
    #[cfg(not(feature = "stats"))]
    pub const fn new(val: T) -> Self {
        Self(lock_api::Mutex::const_new(RawMutex::new(), val))
    }

    /// Creates a new [`Mutex`] bound to this init site's lock class.
    #[inline(always)]
    #[cfg(feature = "stats")]
    #[track_caller]
    pub fn new(val: T) -> Self {
        let stats = klockstat::class_for_init_site(core::panic::Location::caller(), "Mutex");
        Self::new_with_stats(val, stats)
    }

    /// Creates a [`Mutex`] bound to `stats`.
    #[inline(always)]
    #[cfg(feature = "stats")]
    pub const fn new_with_stats(val: T, stats: &'static klockstat::LockClassStats) -> Self {
        Self(lock_api::Mutex::const_new(
            RawMutex::new_with_stats(stats),
            val,
        ))
    }

    /// Creates a [`Mutex`] from a custom [`RawMutex`] and initial value.
    #[inline(always)]
    pub const fn const_new(raw: RawMutex, val: T) -> Self {
        Self(lock_api::Mutex::const_new(raw, val))
    }
}

/// An alias of [`lock_api::MutexGuard`].
pub type MutexGuard<'a, T> = lock_api::MutexGuard<'a, RawMutex, T>;

#[cfg(test)]
mod tests {
    use std::sync::Once;

    use ktask as thread;

    use crate::{Mutex, static_lock};

    static INIT: Once = Once::new();

    fn may_interrupt() {
        // simulate interrupts
        if fastrand::u8(0..3) == 0 {
            thread::yield_now();
        }
    }

    #[test]
    fn lots_and_lots() {
        INIT.call_once(thread::init_scheduler);

        const NUM_TASKS: u32 = 10;
        const NUM_ITERS: u32 = 10_000;
        static_lock! {
            static M: Mutex<u32> = Mutex::new(0);
        }

        fn inc(delta: u32) {
            for _ in 0..NUM_ITERS {
                let mut val = M.lock();
                *val += delta;
                may_interrupt();
                drop(val);
                may_interrupt();
            }
        }

        for _ in 0..NUM_TASKS {
            thread::spawn(|| inc(1));
            thread::spawn(|| inc(2));
        }

        println!("spawn OK");
        loop {
            let val = M.lock();
            if *val == NUM_ITERS * NUM_TASKS * 3 {
                break;
            }
            may_interrupt();
            drop(val);
            may_interrupt();
        }

        assert_eq!(*M.lock(), NUM_ITERS * NUM_TASKS * 3);
        println!("Mutex test OK");
    }
}

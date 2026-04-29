// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! A blocking mutex implementation.

#[cfg(feature = "stats")]
use core::sync::atomic::AtomicU64 as StatsAtomicU64;
use core::sync::atomic::{AtomicU64, Ordering};

use event_listener::{Event, listener};
use ktask::{current, future::block_on};

use crate::util::{Spin, SpinConfig};

/// Statistics for mutex operations (available with `stats` feature).
#[cfg(feature = "stats")]
#[derive(Debug, Default)]
pub struct MutexStats {
    /// Total number of lock acquisitions
    pub total_locks: StatsAtomicU64,
    /// Total number of spin iterations
    pub total_spins: StatsAtomicU64,
    /// Total number of times the task blocked
    pub total_blocks: StatsAtomicU64,
}

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
    stats: MutexStats,
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
            stats: MutexStats {
                total_locks: StatsAtomicU64::new(0),
                total_spins: StatsAtomicU64::new(0),
                total_blocks: StatsAtomicU64::new(0),
            },
        }
    }

    /// Gets the mutex statistics (only available with `stats` feature).
    ///
    /// Returns `(total_locks, total_spins, total_blocks)`.
    #[cfg(feature = "stats")]
    pub fn stats(&self) -> (u64, u64, u64) {
        (
            self.stats.total_locks.load(Ordering::Relaxed),
            self.stats.total_spins.load(Ordering::Relaxed),
            self.stats.total_blocks.load(Ordering::Relaxed),
        )
    }

    /// Resets all statistics counters (only available with `stats` feature).
    ///
    /// Note: This method is not synchronized. If called while the mutex is being
    /// actively used, the reset may produce inconsistent results as the individual
    /// counters are reset independently.
    #[cfg(feature = "stats")]
    pub fn reset_stats(&self) {
        self.stats.total_locks.store(0, Ordering::Relaxed);
        self.stats.total_spins.store(0, Ordering::Relaxed);
        self.stats.total_blocks.store(0, Ordering::Relaxed);
    }
}

impl Default for RawMutex {
    fn default() -> Self {
        Self::new()
    }
}

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
        let current_id = current().id().as_u64();
        let mut spin = Spin::new(self.config);
        let mut owner_id = self.owner_id.load(Ordering::Relaxed);
        #[cfg(feature = "stats")]
        let mut spin_count = 0u64;

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
                        {
                            self.stats
                                .total_spins
                                .fetch_add(spin_count, Ordering::Relaxed);
                        }

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
                #[cfg(feature = "stats")]
                {
                    spin_count += 1;
                }
                owner_id = self.owner_id.load(Ordering::Relaxed);
                continue;
            }

            #[cfg(feature = "stats")]
            {
                self.stats
                    .total_spins
                    .fetch_add(spin_count, Ordering::Relaxed);
                self.stats.total_blocks.fetch_add(1, Ordering::Relaxed);
            }

            listener!(self.event => listener);

            owner_id = self.owner_id.load(Ordering::Acquire);
            if owner_id == 0 {
                continue;
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
        let current_id = current().id().as_u64();
        // The reason for using a strong compare_exchange is explained here:
        // https://github.com/Amanieu/parking_lot/pull/207#issuecomment-575869107
        let acquired = self
            .owner_id
            .compare_exchange(0, current_id, Ordering::Acquire, Ordering::Relaxed)
            .is_ok();
        if acquired {
            #[cfg(feature = "watchdog")]
            current().inner().push_held_lock(self as *const _ as usize);
        }
        acquired
    }

    #[inline(always)]
    unsafe fn unlock(&self) {
        let owner_id = self.owner_id.swap(0, Ordering::Release);
        assert_eq!(
            owner_id,
            current().id().as_u64(),
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

/// An alias of [`lock_api::Mutex`].
pub type Mutex<T> = lock_api::Mutex<RawMutex, T>;
/// An alias of [`lock_api::MutexGuard`].
pub type MutexGuard<'a, T> = lock_api::MutexGuard<'a, RawMutex, T>;

#[cfg(test)]
mod tests {
    use std::sync::Once;

    use ktask as thread;

    use crate::Mutex;

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
        static M: Mutex<u32> = Mutex::new(0);

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

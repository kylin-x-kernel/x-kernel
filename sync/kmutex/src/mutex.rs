// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! A blocking mutex implementation.

use core::sync::atomic::{AtomicU64, Ordering};

use event_listener::{Event, listener};
use kiface_task_api::TaskExecutionIf;
#[cfg(feature = "watchdog")]
use kiface_task_api::TaskLockDiagnosticsIf;

use crate::{SpinConfig, spin::Spin};

/// A [`lock_api::RawMutex`] implementation.
///
/// When the mutex is locked, the current task will block and be put into the
/// wait queue after adaptive spinning. Unlock wakes one listener.
/// Requires an installed task provider; blocking acquisition requires
/// sleepable task context. Recursive blocking acquisition panics.
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

    /// Returns `true` if the current task owns this mutex.
    #[inline(always)]
    pub fn is_owned_by_current(&self) -> bool {
        self.owner_id.load(Ordering::Relaxed) == TaskExecutionIf::current_owner_key().get()
    }

    /// Asserts that the current task does not own this mutex.
    ///
    /// # Panics
    /// Panics with `message` and the numeric owner key if this task owns it.
    #[inline(always)]
    pub fn assert_not_owned_by_current(&self, message: &str) {
        assert!(
            !self.is_owned_by_current(),
            "{}: {}",
            TaskExecutionIf::current_owner_key().get(),
            message
        );
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

// SAFETY: only a successful Acquire CAS from zero grants ownership; Release
// unlock publishes protected writes before waking a registered listener.
// The provider supplies stable nonzero task keys, and GuardNoSend keeps unlock
// on the acquiring task even when that task migrates to another CPU.
unsafe impl lock_api::RawMutex for RawMutex {
    type GuardMarker = lock_api::GuardNoSend;

    /// Initial value for an unlocked mutex.
    ///
    /// A “non-constant” const item is a legacy way to supply an initialized
    /// value to downstream static items. Can hopefully be replaced with
    /// `const fn new() -> Self` at some point.
    const INIT: Self = RawMutex::new();

    #[inline(always)]
    fn lock(&self) {
        // The owner key remains valid across blocking and CPU migration.
        let current_id = TaskExecutionIf::current_owner_key().get();
        let mut spin = Spin::new(self.config);
        let mut owner_id = self.owner_id.load(Ordering::Relaxed);
        #[cfg(feature = "stats")]
        let mut did_block = false;

        loop {
            assert_ne!(
                owner_id, current_id,
                "{} tried to acquire mutex it already owns.",
                current_id
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
                        TaskLockDiagnosticsIf::lock_acquired(self as *const _ as usize);
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
            TaskLockDiagnosticsIf::lock_waiting(self as *const _ as usize);
            TaskExecutionIf::block_on(core::pin::pin!(listener));
            owner_id = self.owner_id.load(Ordering::Acquire);
        }
    }

    #[inline(always)]
    fn try_lock(&self) -> bool {
        let current_id = TaskExecutionIf::current_owner_key().get();
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
            TaskLockDiagnosticsIf::lock_acquired(self as *const _ as usize);
        }
        acquired
    }

    #[inline(always)]
    unsafe fn unlock(&self) {
        // SAFETY: `lock_api` only calls `unlock` after this mutex instance was
        // successfully locked by the current guard holder, so releasing the
        // owner slot and waking one waiter upholds the raw mutex protocol.
        let owner_id = self.owner_id.swap(0, Ordering::Release);
        let current_id = TaskExecutionIf::current_owner_key().get();
        assert_eq!(
            owner_id, current_id,
            "{} tried to release mutex it doesn't own",
            current_id
        );
        #[cfg(feature = "watchdog")]
        TaskLockDiagnosticsIf::lock_released(self as *const _ as usize);
        self.event.notify(1);
    }

    #[inline(always)]
    fn is_locked(&self) -> bool {
        self.owner_id.load(Ordering::Relaxed) != 0
    }
}

/// A kernel mutex built on [`lock_api::Mutex`].
///
/// Guards stay on the acquiring task; that task may migrate between CPUs.
/// Blocking acquisition requires sleepable context and panics on recursion.
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

    /// Returns `true` if the current task owns this mutex.
    #[inline(always)]
    pub fn is_owned_by_current(&self) -> bool {
        // SAFETY: This wrapper only reads the raw mutex owner metadata and
        // does not use the raw lock to bypass `lock_api`'s locking protocol.
        unsafe { self.0.raw().is_owned_by_current() }
    }

    /// Asserts that the current task does not own this mutex.
    ///
    /// # Panics
    /// Panics with `message` and the numeric owner key if this task owns it.
    #[inline(always)]
    pub fn assert_not_owned_by_current(&self, message: &str) {
        // SAFETY: This wrapper only reads the raw mutex owner metadata and
        // does not use the raw lock to bypass `lock_api`'s locking protocol.
        unsafe { self.0.raw().assert_not_owned_by_current(message) };
    }
}

/// A task-bound [`lock_api::MutexGuard`]; it cannot be sent to another task.
///
/// This type-only check requires no scheduler provider:
///
/// ```compile_fail,E0277
/// fn require_send<T: Send>() {}
/// require_send::<kmutex::MutexGuard<'static, ()>>();
/// ```
pub type MutexGuard<'a, T> = lock_api::MutexGuard<'a, RawMutex, T>;

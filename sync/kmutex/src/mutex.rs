// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! A blocking mutex implementation.

use core::sync::atomic::{AtomicU64, Ordering};

use intrusive_collections::UnsafeRef;
use kiface_task_api::TaskExecutionIf;
#[cfg(feature = "watchdog")]
use kiface_task_api::TaskLockDiagnosticsIf;
use kspin::SpinNoPreempt;
use ktask_types::OwnerKey;

use crate::{
    SpinConfig,
    spin::Spin,
    wait::{WaitFuture, WaitState, Waiter},
};

/// Linux `MUTEX_FLAG_WAITERS`: the low bit of the owner word, set while the
/// wait list is non-empty.
const WAITERS_FLAG: u64 = 1 << 0;
// The waiters flag must stay inside the owner key's reserved flag field.
const _: () = assert!(WAITERS_FLAG & !OwnerKey::FLAG_MASK == 0);

/// A [`lock_api::RawMutex`] implementation.
///
/// When the mutex is locked, the current task will block and be put into an
/// owned FIFO wait queue after adaptive spinning. Unlock wakes at most one
/// waiter, and only while the queue is non-empty.
/// Requires an installed task provider; blocking acquisition requires
/// sleepable task context. Recursive blocking acquisition panics.
pub struct RawMutex {
    /// Owner word: an aligned owner key with [`WAITERS_FLAG`] in its low bit,
    /// or `0` when unlocked; the higher flag bits of [`OwnerKey::FLAG_MASK`]
    /// are reserved.
    owner_id: AtomicU64,
    /// Guard for the wait list; every list mutation and the in-word waiters
    /// flag transition happens while holding this lock.
    wait_state: SpinNoPreempt<WaitState>,
    config: SpinConfig,
    #[cfg(feature = "stats")]
    stats: &'static klockstat::LockClassStats,
}

/// The raw owner word stored in the mutex owner slot: an aligned owner key
/// with lock-word flags packed into its low bits. Zero is the unlocked word.
#[derive(Clone, Copy)]
struct OwnerWord(u64);

impl OwnerWord {
    /// Returns the owner key with the flag bits masked off.
    ///
    /// Returns: `Some(key)` for a locked word, `None` when the owner-key part
    /// is zero.
    fn key(self) -> Option<OwnerKey> {
        OwnerKey::from_aligned(self.0 & !OwnerKey::FLAG_MASK)
    }

    /// Returns the reserved lock-word flag bits.
    fn flags(self) -> u64 {
        self.0 & OwnerKey::FLAG_MASK
    }

    /// Reports whether the waiters flag is set in this word snapshot.
    ///
    /// Returns: `true` when [`WAITERS_FLAG`] is set, `false` otherwise.
    fn has_waiters(self) -> bool {
        self.flags() & WAITERS_FLAG != 0
    }
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
            owner_id: AtomicU64::new(0),
            wait_state: SpinNoPreempt::new(WaitState::new()),
            config,
            #[cfg(feature = "stats")]
            stats: &klockstat::NOOP_CLASS,
        }
    }

    /// Returns `true` if the current task owns this mutex.
    #[inline(always)]
    pub fn is_owned_by_current(&self) -> bool {
        OwnerWord(self.owner_id.load(Ordering::Relaxed)).key()
            == Some(TaskExecutionIf::current_owner_key())
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
            TaskExecutionIf::current_owner_key().as_raw(),
            message
        );
    }

    /// Rewrites the in-word waiter flag from the guarded wait-list state.
    ///
    /// - `st`: the wait-state guard whose list is mirrored; read, not mutated.
    ///
    /// Locking: caller holds `wait_state`, which serializes every flag writer
    /// (an unlock's `fetch_and(FLAG_MASK)` preserves the flag), so this plain
    /// read cannot race a concurrent change. The flag is written only when the
    /// list's emptiness flipped, so the common already-consistent mutation
    /// avoids an atomic RMW.
    fn sync_waiters_flag(&self, st: &mut WaitState) {
        let want = !st.list.is_empty();
        if OwnerWord(self.owner_id.load(Ordering::Relaxed)).has_waiters() != want {
            if want {
                self.set_waiters();
            } else {
                self.clear_waiters();
            }
        }
    }

    /// Sets the in-word waiters flag.
    ///
    /// Locking: caller must hold `wait_state`, which serializes every flag
    /// writer; this update is not atomic with the list change it mirrors.
    fn set_waiters(&self) {
        self.owner_id.fetch_or(WAITERS_FLAG, Ordering::AcqRel);
    }

    /// Clears the in-word waiters flag.
    ///
    /// Locking: caller must hold `wait_state`, which serializes every flag
    /// writer; this update is not atomic with the list change it mirrors.
    fn clear_waiters(&self) {
        self.owner_id.fetch_and(!WAITERS_FLAG, Ordering::AcqRel);
    }

    /// Attempts a Linux `__mutex_trylock_common`-style acquisition.
    ///
    /// - `current`: the calling task's flag-reserved owner key.
    ///
    /// Returns: `true` whenever the owner-key part is zero, preserving any set
    /// flag bits in the resulting word; `false` while another task owns it.
    fn try_acquire(&self, current: OwnerKey) -> bool {
        let mut word = self.owner_id.load(Ordering::Relaxed);
        loop {
            let owner = OwnerWord(word);
            if owner.key().is_some() {
                return false;
            }
            let next = current.as_raw() | owner.flags();
            match self.owner_id.compare_exchange_weak(
                word,
                next,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(new_word) => word = new_word,
            }
        }
    }

    /// Pops the oldest waiter and wakes it.
    ///
    /// Locking: takes `wait_state` only to pop and re-publish the waiter flag;
    /// the wake runs after the lock is released and may reschedule the caller.
    fn unlock_slowpath(&self) {
        let node = {
            let mut st = self.wait_state.lock();
            let Some(node) = st.list.pop_front() else {
                self.sync_waiters_flag(&mut st);
                return;
            };
            self.sync_waiters_flag(&mut st);
            node
        };
        // `node` was unlinked under the wait lock and the guard is gone, so other
        // admissions can proceed before the wake. `latch` is the last access to
        // the node: once it publishes the wake, the waiter may drop its frame.
        let waker = node.latch();
        if let Some(w) = waker {
            w.wake_by_ref();
        }
    }
}

#[cfg(feature = "stats")]
impl RawMutex {
    /// Creates a [`RawMutex`] bound to `stats`.
    #[inline(always)]
    pub const fn new_with_stats(stats: &'static klockstat::LockClassStats) -> Self {
        Self::with_config_and_stats(
            SpinConfig {
                max_spins: 10,
                spin_before_yield: 3,
            },
            stats,
        )
    }

    const fn with_config_and_stats(
        config: SpinConfig,
        stats: &'static klockstat::LockClassStats,
    ) -> Self {
        Self {
            owner_id: AtomicU64::new(0),
            wait_state: SpinNoPreempt::new(WaitState::new()),
            config,
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

// SAFETY: only a successful Acquire CAS from a cleared owner part grants
// ownership; the unlock Release swap publishes protected writes before waking
// a registered waiter. The provider supplies stable, flag-reserved task keys
// (low bits zero), and GuardNoSend keeps unlock on the acquiring task even
// when that task migrates to another CPU.
unsafe impl lock_api::RawMutex for RawMutex {
    type GuardMarker = lock_api::GuardNoSend;

    /// Initial value for an unlocked mutex.
    ///
    /// A “non-constant” const item is a legacy way to supply an initialized
    /// value to downstream static items. Can hopefully be replaced with
    /// `const fn new() -> Self` at some point.
    const INIT: Self = RawMutex::new();

    /// Acquires the mutex, spinning adaptively and then blocking.
    ///
    /// Locking: takes `wait_state` only to queue or re-check the owner and
    /// blocks after releasing it. Requires sleepable task context.
    ///
    /// # Panics
    ///
    /// Panics when the calling task already owns the mutex.
    #[inline(always)]
    fn lock(&self) {
        // The owner key remains valid across blocking and CPU migration.
        let current_id = TaskExecutionIf::current_owner_key();
        let current_raw = current_id.as_raw();
        let mut spin = Spin::new(self.config);
        let mut owner_id = self.owner_id.load(Ordering::Relaxed);
        #[cfg(feature = "stats")]
        let mut did_block = false;

        loop {
            assert_ne!(
                OwnerWord(owner_id).key(),
                Some(current_id),
                "{} tried to acquire mutex it already owns.",
                current_raw
            );

            if OwnerWord(owner_id).key().is_none() {
                let next = current_raw | OwnerWord(owner_id).flags();
                match self.owner_id.compare_exchange_weak(
                    owner_id,
                    next,
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

            // Park path: own the wait list so "has waiters" stays authoritative.
            let node = core::pin::pin!(Waiter::new());
            let node_ref = node.as_ref().get_ref();

            {
                let mut st = self.wait_state.lock();

                // Re-check under the wait lock: an unlock that raced our spin
                // may have released ownership since the last owner read.
                if self.try_acquire(current_id) {
                    // Drop the wait lock before the diagnostic hook: hooks run
                    // while `wait_state` is not held.
                    drop(st);
                    #[cfg(feature = "stats")]
                    self.record_acquisitions();
                    #[cfg(feature = "watchdog")]
                    TaskLockDiagnosticsIf::lock_acquired(self as *const _ as usize);
                    break;
                }

                // Still held: only here does this attempt enqueue itself and sleep.
                // SAFETY: the node is pinned on this frame and shared access is
                // confined to this `wait_state` critical section.
                unsafe {
                    st.list.push_back(UnsafeRef::from_raw(node_ref));
                }
                self.sync_waiters_flag(&mut st);

                // The flag RMW and this load touch the same word, so a zero
                // owner part here means an unlock beat our flag publish: retry
                // instead of sleeping on a queue nobody will wake.
                let owner = self.owner_id.load(Ordering::Acquire);
                if OwnerWord(owner).key().is_none() {
                    // SAFETY: the node was just linked and is still owned by this
                    // frame; the wait lock excludes any concurrent unlink.
                    unsafe {
                        st.list.cursor_mut_from_ptr(node_ref).remove();
                    }
                    self.sync_waiters_flag(&mut st);
                    owner_id = 0;
                    continue;
                }
                drop(st);
            }

            #[cfg(feature = "stats")]
            if !did_block {
                self.record_blocking();
                did_block = true;
            }

            #[cfg(feature = "watchdog")]
            TaskLockDiagnosticsIf::lock_waiting(self as *const _ as usize);
            // SAFETY: the future borrows the pinned node and resolves only once
            // the latch is set, which the unlocker does only after unlinking.
            let future = WaitFuture::new(node_ref);
            TaskExecutionIf::block_on(core::pin::pin!(future));
            owner_id = self.owner_id.load(Ordering::Acquire);
        }
    }

    #[inline(always)]
    fn try_lock(&self) -> bool {
        let current_id = TaskExecutionIf::current_owner_key();
        let acquired = self.try_acquire(current_id);
        if acquired {
            #[cfg(feature = "stats")]
            self.record_acquisitions();
            #[cfg(feature = "watchdog")]
            TaskLockDiagnosticsIf::lock_acquired(self as *const _ as usize);
        }
        acquired
    }

    /// Releases the mutex and wakes the oldest waiter when one is queued.
    ///
    /// Locking: takes `wait_state` only while the waiter flag may be set; the
    /// wake runs after the lock is released.
    #[inline(always)]
    unsafe fn unlock(&self) {
        // SAFETY: `lock_api` only calls `unlock` after this mutex instance was
        // successfully locked by the current guard holder, so releasing the
        // owner slot and waking one waiter upholds the raw mutex protocol.
        // Clear only the owner-key part, keeping the flag bits (Linux
        // `__owner_flags`): a task that barges in and out during this fast
        // path then inherits `WAITERS_FLAG` and its release wakes a waiter.
        let owner_word = self
            .owner_id
            .fetch_and(OwnerKey::FLAG_MASK, Ordering::Release);
        // `GuardNoSend` keeps the guard on the acquiring task, so releasing
        // from another task is only reachable through raw-API misuse, not a
        // legitimate failure. Keep the owner-key lookup out of release builds:
        // it is a cross-crate provider call on every unlock.
        debug_assert_eq!(
            OwnerWord(owner_word).key(),
            Some(TaskExecutionIf::current_owner_key()),
            "a task may only release a mutex it owns"
        );
        #[cfg(feature = "watchdog")]
        TaskLockDiagnosticsIf::lock_released(self as *const _ as usize);
        // Wake one waiter only while the list may still be non-empty. The flag
        // survived the owner clear above, so single-word coherence with the
        // admission's publish-and-reread still closes the lost-wakeup window.
        if OwnerWord(owner_word).has_waiters() {
            self.unlock_slowpath();
        }
    }

    #[inline(always)]
    fn is_locked(&self) -> bool {
        OwnerWord(self.owner_id.load(Ordering::Relaxed))
            .key()
            .is_some()
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

    /// Creates a mutex with a caller-selected bounded spin policy.
    ///
    /// `config` has the same bounds and backoff semantics as [`SpinConfig`].
    /// Blocking acquisition still requires sleepable task context. Equal
    /// `max_spins` and `spin_before_yield` values skip voluntary yields during
    /// the spin budget, then use the ordinary blocking wait queue.
    #[inline(always)]
    #[cfg(not(feature = "stats"))]
    pub const fn with_config(val: T, config: SpinConfig) -> Self {
        Self(lock_api::Mutex::const_new(
            RawMutex::with_config(config),
            val,
        ))
    }

    /// Creates a configured mutex bound to this init site's lock class.
    ///
    /// `config` has the same bounds and backoff semantics as [`SpinConfig`].
    /// Blocking acquisition still requires sleepable task context. Equal
    /// `max_spins` and `spin_before_yield` values skip voluntary yields during
    /// the spin budget, then use the ordinary blocking wait queue.
    #[inline(always)]
    #[cfg(feature = "stats")]
    #[track_caller]
    pub fn with_config(val: T, config: SpinConfig) -> Self {
        let stats = klockstat::class_for_init_site(core::panic::Location::caller(), "Mutex");
        Self(lock_api::Mutex::const_new(
            RawMutex::with_config_and_stats(config, stats),
            val,
        ))
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

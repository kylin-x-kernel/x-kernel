// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! One-time initialization primitive.
//!
//! [`Once<T>`] provides atomic lazy initialization suitable for `no_std`
//! statics. Replaces the need for `spin::Mutex` or `klazy` in this crate.
//!
//! After initialization, [`Once::get()`] returns `Option<&T>` via an
//! `Acquire` load — no lock, no contention on the read path.

use core::{cell::UnsafeCell, mem::MaybeUninit, sync::atomic::Ordering};

use portable_atomic::AtomicU8;

/// State values stored in `Once::state`.
const UNINIT: u8 = 0;
const INITIALIZING: u8 = 1;
const READY: u8 = 2;
const POISONED: u8 = 3;

/// A primitive for one-time initialization of a value `T`.
///
/// Initialization is serialized via a CAS state machine; reads after
/// initialization are a single atomic load. Spin-wait uses
/// `core::hint::spin_loop` (no `spin` crate dependency).
pub(crate) struct Once<T> {
    state: AtomicU8,
    storage: UnsafeCell<MaybeUninit<T>>,
}

// SAFETY:
// - `storage` is only written by the thread that successfully CASes
//   `UNINIT -> INITIALIZING`. Other threads either spin until `READY`
//   or observe `READY` via `Acquire` load before reading.
// - After `READY`, `storage` is immutable and only `&T` references are
//   handed out.
// - Therefore sharing `&Once<T>` across threads is sound when `T: Send + Sync`.
unsafe impl<T: Send + Sync> Sync for Once<T> {}
// SAFETY: moving `Once<T>` between threads is sound because the atomic
// state machine serializes initialization, and `T` may be transferred
// when `T: Send`.
unsafe impl<T: Send> Send for Once<T> {}

impl<T> Once<T> {
    /// Creates a new uninitialized `Once`.
    pub(crate) const fn new() -> Self {
        Self {
            state: AtomicU8::new(UNINIT),
            storage: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }

    /// Returns `Some(&T)` if initialization has completed, `None` otherwise.
    ///
    /// Non-blocking, non-panicking. Single `Acquire` load.
    pub(crate) fn get(&self) -> Option<&T> {
        if self.state.load(Ordering::Acquire) == READY {
            // SAFETY: `READY` is only published after `storage` is fully
            // initialized. We hand out only `&T`; no further mutation occurs.
            // MaybeUninit<T> shares T's layout, so the cast retains alignment.
            Some(unsafe { &*self.storage.get().cast::<T>() })
        } else {
            None
        }
    }

    /// Lazily initializes the value, returning `&T`.
    ///
    /// If this thread performs initialization and `f` returns `Err`, the
    /// cell is reset to `UNINIT` and may be retried. If another thread
    /// is initializing, this thread spin-waits.
    ///
    /// Panics if a previous initialization attempt poisoned the cell.
    pub(crate) fn get_or_try_init<E, F: FnOnce() -> Result<T, E>>(&self, f: F) -> Result<&T, E> {
        // Fast path.
        if let Some(value) = self.get() {
            return Ok(value);
        }
        self.init_slow(f)
    }

    #[cold]
    fn init_slow<E, F: FnOnce() -> Result<T, E>>(&self, f: F) -> Result<&T, E> {
        loop {
            match self.state.compare_exchange(
                UNINIT,
                INITIALIZING,
                Ordering::Acquire,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    // We own initialization.
                    let result = f();
                    match result {
                        Ok(value) => {
                            // SAFETY: we hold exclusive initialization rights.
                            unsafe {
                                (*self.storage.get()).write(value);
                            }
                            self.state.store(READY, Ordering::Release);
                            // SAFETY: This thread initialized T and just published READY;
                            // the cell keeps it live and immutable. MaybeUninit<T>
                            // shares T's layout and alignment.
                            return Ok(unsafe { &*self.storage.get().cast::<T>() });
                        }
                        Err(e) => {
                            // Allow retry.
                            self.state.store(UNINIT, Ordering::Release);
                            return Err(e);
                        }
                    }
                }
                Err(actual) => match actual {
                    READY => {
                        // SAFETY: The failed CAS acquired the other initializer's
                        // READY publication. T remains live and immutable in the
                        // cell; MaybeUninit<T> has T's layout and alignment.
                        return Ok(unsafe { &*self.storage.get().cast::<T>() });
                    }
                    #[expect(
                        clippy::panic,
                        reason = "CG-021-no-panic-on-reachable-failure: A previous initializer \
                                  panicked and left no value; like std's poisoned Once and \
                                  LazyLock, the value can never be produced"
                    )]
                    POISONED => {
                        panic!("Once poisoned by a previous panic during initialization")
                    }
                    INITIALIZING | UNINIT => {
                        // Contention or spurious failure; spin and re-check.
                        core::hint::spin_loop();
                    }
                    _ => core::hint::spin_loop(),
                },
            }
        }
    }
}

impl<T> Drop for Once<T> {
    fn drop(&mut self) {
        // We have `&mut self`, no atomics needed.
        if *self.state.get_mut() == READY {
            // SAFETY: READY guarantees storage holds a live `T`.
            unsafe {
                self.storage.get_mut().assume_init_drop();
            }
        }
    }
}

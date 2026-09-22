// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Scheduled task: caller inner data plus class and attribute state.

#![expect(
    dead_code,
    reason = "RQ-protected seat/attr accessors wire up with the class arbitrator next."
)]

use core::{
    cell::UnsafeCell,
    ops::Deref,
    sync::atomic::{AtomicU8, Ordering},
};

use crate::policy::{SchedAttr, SchedClass, SchedError};

/// Zero-sized token: RQ-protected fields may be read or written.
///
/// Constructed by [`SchedTask::init_attr_unpublished`] for a still-private
/// task, or later by a run-queue owner that already holds the RQ lock.
#[derive(Clone, Copy)]
pub(crate) struct RqLocked(());

impl RqLocked {
    /// # Safety
    ///
    /// The caller holds the owning run-queue lock for this task (or is the
    /// exclusive creator of an unpublished task).
    pub(crate) const unsafe fn assume() -> Self {
        Self(())
    }
}

/// Interior cell mutated only while the owning RQ lock is held.
#[repr(transparent)]
pub(crate) struct RqCell<V>(UnsafeCell<V>);

// SAFETY: every `RqCell` method is on `impl<V: Copy>`, and writes go through
// `RqLocked`. Concurrent readers without the token are not provided.
// `V: Send + Copy` matches that API: `Send` lets the value move with the
// lock across CPUs; `Copy` is what the cell actually stores.
unsafe impl<V: Send + Copy> Sync for RqCell<V> {}

impl<V: Copy> RqCell<V> {
    pub const fn new(v: V) -> Self {
        Self(UnsafeCell::new(v))
    }

    pub fn get(&self, _: RqLocked) -> V {
        // SAFETY: deref of `UnsafeCell::get()` (`*mut V`) for a read.
        // Validity and alignment: `RqCell` is `#[repr(transparent)]` over
        // `UnsafeCell<V>`, so `get()` yields a pointer to this cell's `V`.
        // Initialization: `new` stores a fully initialized `V`; `set` only
        // replaces it with another complete `V`.
        // No concurrent write: `RqLocked` means the RQ lock is held, so no
        // other CPU may `set` this cell. Exclusive write access is not
        // required for a `Copy` load.
        unsafe { *self.0.get() }
    }

    pub fn set(&self, v: V, _: RqLocked) {
        // SAFETY: write through `UnsafeCell::get()` (`*mut V`).
        // Validity and alignment: same transparent layout as `get`.
        // Initialization: the destination is a live `V` from `new` or a
        // previous `set`; the stored value is a complete `Copy` `V`.
        // Exclusive write: `RqLocked` means the RQ lock is held, so no
        // concurrent `get` or `set` on this cell.
        unsafe {
            *self.0.get() = v;
        }
    }
}

/// Which ready structure currently holds this task.
///
/// `Detached` is 0 so it never collides with a class seat. The class
/// variants follow the same rank order as [`SchedClass`]
/// (Stop > Deadline > Rt > Fair > Idle), offset by 1. Do not use
/// [`SchedClass::as_u8`] as a seat byte; convert with
/// [`QueueSeat::from_sched_class`].
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QueueSeat {
    Detached = 0,
    Stop     = 1,
    Deadline = 2,
    Rt       = 3,
    Fair     = 4,
    Idle     = 5,
}

impl QueueSeat {
    const fn from_u8(raw: u8) -> Self {
        match raw {
            1 => Self::Stop,
            2 => Self::Deadline,
            3 => Self::Rt,
            4 => Self::Fair,
            5 => Self::Idle,
            _ => Self::Detached,
        }
    }

    /// Seat for `c`'s class queue. Not [`SchedClass::as_u8`]: that encoding
    /// has no `Detached` and would map Fair (3) onto [`Self::Rt`].
    const fn from_sched_class(c: SchedClass) -> Self {
        match c {
            SchedClass::Stop => Self::Stop,
            SchedClass::Deadline => Self::Deadline,
            SchedClass::Rt => Self::Rt,
            SchedClass::Fair => Self::Fair,
            SchedClass::Idle => Self::Idle,
        }
    }

    const fn as_u8(self) -> u8 {
        self as u8
    }
}

/// Kernel-only class used at task construction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KernelClass {
    /// Per-CPU idle thread.
    Idle,
}

impl KernelClass {
    const fn sched_class(self) -> SchedClass {
        match self {
            Self::Idle => SchedClass::Idle,
        }
    }
}

/// Scheduled task: caller-owned inner data plus class and attributes.
pub struct SchedTask<T> {
    inner: T,
    class: AtomicU8,
    seat: RqCell<QueueSeat>,
    attr: RqCell<SchedAttr>,
}

impl<T> SchedTask<T> {
    /// Wraps `inner` as a Fair task with [`SchedAttr::DEFAULT`].
    pub const fn new(inner: T) -> Self {
        Self::new_with_class(inner, SchedClass::Fair, SchedAttr::DEFAULT)
    }

    /// Wraps `inner` as a kernel-only Idle task.
    pub const fn new_kernel(inner: T, class: KernelClass) -> Self {
        Self::new_with_class(inner, class.sched_class(), SchedAttr::DEFAULT)
    }

    const fn new_with_class(inner: T, class: SchedClass, attr: SchedAttr) -> Self {
        Self {
            inner,
            class: AtomicU8::new(class.as_u8()),
            seat: RqCell::new(QueueSeat::Detached),
            attr: RqCell::new(attr),
        }
    }

    /// Returns a reference to the inner task struct.
    pub const fn inner(&self) -> &T {
        &self.inner
    }

    /// Live class. Acquire so a lock-free CPU-selection read sees the last
    /// Release store from `set_class`.
    pub fn sched_class(&self) -> SchedClass {
        SchedClass::from_u8(self.class.load(Ordering::Acquire))
    }

    /// `true` when this task is the per-CPU idle class.
    pub fn is_idle_class(&self) -> bool {
        self.sched_class() == SchedClass::Idle
    }

    /// Write attr and class on a task that has not been published yet.
    ///
    /// # Errors
    ///
    /// Returns the first [`SchedAttr::validate`] failure.
    pub(crate) fn init_attr_unpublished(&self, attr: SchedAttr) -> Result<(), SchedError> {
        attr.validate()?;
        // SAFETY: the creator still exclusively owns this unpublished task;
        // it is not on any run queue and no other CPU can observe it.
        let tok = unsafe { RqLocked::assume() };
        self.attr.set(attr, tok);
        self.class.store(attr.class().as_u8(), Ordering::Release);
        Ok(())
    }

    pub(crate) fn attr(&self, tok: RqLocked) -> SchedAttr {
        self.attr.get(tok)
    }

    pub(crate) fn set_attr(&self, attr: SchedAttr, tok: RqLocked) {
        self.attr.set(attr, tok);
    }

    pub(crate) fn set_class(&self, class: SchedClass, _tok: RqLocked) {
        self.class.store(class.as_u8(), Ordering::Release);
    }

    pub(crate) fn seat(&self, tok: RqLocked) -> QueueSeat {
        self.seat.get(tok)
    }

    pub(crate) fn set_seat(&self, seat: QueueSeat, tok: RqLocked) {
        self.seat.set(seat, tok);
    }
}

impl<T> Deref for SchedTask<T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

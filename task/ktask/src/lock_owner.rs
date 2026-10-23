// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Lock-holder records for optimistic spinning in sleepable locks.
//!
//! Linux keeps spinning on a contended rwsem while its writer is on a CPU
//! (`rwsem_spin_on_owner` reads `owner->on_cpu` under RCU). Tasks here are not
//! RCU-freed, so a spinner must not dereference the holder. A record keeps the
//! holder's [`holder_key`] and the CPU it acquired on, and every CPU publishes
//! the key of the task it runs. Spinners only compare keys, so a stale record
//! misjudges whether spinning pays off. The same key is the re-entry panic, so
//! a reused `TaskInner` address can also panic a later task. That matches
//! `RawMutex`, whose `owner_key` is the address for the same tasks.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::{TaskInner, api::current_may_uninit};

/// Non-zero identity of `task` in lock-owner records and running-task slots.
///
/// [`TaskInner::owner_key`] can be 0 for a user runtime whose identity token
/// is unknown, and 0 means "no holder" here. Such a task falls back to its
/// `TaskInner` address, which is what `owner_key` already is for kernel, idle,
/// and internal tasks.
#[inline]
pub(crate) fn holder_key(task: &TaskInner) -> u64 {
    match task.owner_key() {
        0 => task as *const TaskInner as usize as u64,
        key => key,
    }
}

/// Where the recorded holder of a [`LockOwner`] is, for a spinner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockOwnerState {
    /// No holder is recorded: the lock is free, held by readers, or a writer
    /// is between acquiring and recording itself.
    Unowned,
    /// The recorded holder is running on the CPU it acquired on.
    Running,
    /// The recorded holder is not running there: preempted, blocked, or
    /// migrated. On a uniprocessor build every recorded holder reads this way.
    NotRunning,
}

/// Exclusive holder of a sleepable lock, recorded for optimistic spinning.
///
/// A hint, not a synchronization primitive: neither store is ordered with the
/// lock word, so callers re-validate through the lock state itself.
#[derive(Debug)]
pub struct LockOwner {
    /// Holder's [`holder_key`]; 0 is [`LockOwnerState::Unowned`].
    owner_key: AtomicU64,
    cpu: AtomicU32,
}

impl LockOwner {
    /// Creates an empty record.
    pub const fn new() -> Self {
        Self {
            owner_key: AtomicU64::new(0),
            cpu: AtomicU32::new(0),
        }
    }

    /// Records the current task as the holder. Call right after acquiring.
    ///
    /// Does nothing before the scheduler installs a current task.
    #[inline]
    pub fn set_current(&self) {
        let Some(task) = current_may_uninit() else {
            return;
        };
        self.cpu.store(
            khal::percpu::this_cpu_id().as_usize() as u32,
            Ordering::Relaxed,
        );
        self.owner_key.store(holder_key(&task), Ordering::Release);
    }

    /// Clears the record. Call right before releasing.
    #[inline]
    pub fn clear(&self) {
        self.owner_key.store(0, Ordering::Release);
    }

    /// Whether the recorded [`holder_key`] is the calling task's.
    ///
    /// An address-valued key can match a later task that reused the
    /// `TaskInner` address. Spinning then misjudges the holder. The re-entry
    /// panic uses this same comparison, as `RawMutex` does with `owner_key`.
    #[inline]
    pub fn is_current(&self) -> bool {
        let key = self.owner_key.load(Ordering::Relaxed);
        key != 0 && current_may_uninit().is_some_and(|task| holder_key(&task) == key)
    }

    /// Classifies the recorded holder.
    #[inline]
    pub fn state(&self) -> LockOwnerState {
        let key = self.owner_key.load(Ordering::Acquire);
        if key == 0 {
            LockOwnerState::Unowned
        } else if is_running_on(self.cpu.load(Ordering::Relaxed), key) {
            LockOwnerState::Running
        } else {
            LockOwnerState::NotRunning
        }
    }
}

impl Default for LockOwner {
    fn default() -> Self {
        Self::new()
    }
}

// `holder_key` (truncated to `usize`) of the task running on this CPU, or 0
// before the scheduler publishes one. Spinners read other CPUs through
// `kcpu_slot::area`; the area stride keeps CPUs off one cache line.
#[cfg(feature = "smp")]
kcpu_slot::cpu_slot_cell! {
    static CPU_RUNNING_TASK: core::sync::atomic::AtomicUsize =
        core::sync::atomic::AtomicUsize::new(0);
}

/// Records the [`holder_key`] of the task about to run on `cpu`.
///
/// Called at CPU bring-up and on every context switch, before the task runs.
/// A CPU whose slot area is not published yet is skipped; no spinner can be
/// running then.
#[cfg(feature = "smp")]
#[inline]
pub(crate) fn note_cpu_running_task(cpu: kcpu_id_map::LogicalCpuId, key: u64) {
    let Some(area) = kcpu_slot::area(cpu) else {
        return;
    };
    CPU_RUNNING_TASK
        .shared_at(area)
        .store(key as usize, Ordering::Relaxed);
}

#[cfg(feature = "smp")]
#[inline]
fn is_running_on(cpu: u32, key: u64) -> bool {
    kcpu_slot::area(kcpu_id_map::LogicalCpuId::new(cpu as usize)).is_some_and(|area| {
        CPU_RUNNING_TASK.shared_at(area).load(Ordering::Relaxed) == key as usize
    })
}

#[cfg(not(feature = "smp"))]
#[inline]
fn is_running_on(_cpu: u32, _owner_key: u64) -> bool {
    false
}

/// Whether a contended acquire can gain anything by spinning.
///
/// On one CPU the holder cannot run until the waiter blocks. `ksync` has no
/// `smp` feature of its own; this follows `ktask`'s.
#[inline]
pub const fn can_optimistic_spin() -> bool {
    cfg!(feature = "smp")
}

unittest::path_mod_test!(tests, "../ut-tests/lock_owner.rs");

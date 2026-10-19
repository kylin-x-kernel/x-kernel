// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! CPU-local watching transitions, independent of preempted task readers.

use core::{
    marker::PhantomData,
    sync::atomic::{AtomicUsize, Ordering, fence},
};

/// Odd sequences denote watched kernel execution; even sequences denote an
/// extended quiescent state. Remote samples alone never complete a grace period.
pub struct CpuWatching {
    sequence: AtomicUsize,
    #[cfg(any(test, unittest))]
    interrupt_depth: AtomicUsize,
}

impl CpuWatching {
    /// Creates watched CPU state, usable before the GP service starts.
    pub const fn new() -> Self {
        Self {
            sequence: AtomicUsize::new(1),
            #[cfg(any(test, unittest))]
            interrupt_depth: AtomicUsize::new(0),
        }
    }

    pub(crate) fn sample(&self) -> usize {
        self.sequence.load(Ordering::Acquire)
    }

    /// Reports watching state, not permission to reclaim an object.
    pub fn is_watching(&self) -> bool {
        self.sample() & 1 != 0
    }

    /// Test-only pairing observation; never a GP completion condition.
    #[cfg(any(test, unittest))]
    pub fn interrupt_depth_for_test(&self) -> usize {
        self.interrupt_depth.load(Ordering::Relaxed)
    }

    /// Test-only boundary observation; equality is not a GP completion proof.
    #[cfg(any(test, unittest))]
    pub fn sequence_for_test(&self) -> usize {
        self.sample()
    }

    fn cross_boundary(&self) {
        // RMW preserves crossings made by a nested interrupt between a local
        // state sample and this update. Fences order protected accesses on both
        // sides, including the activation-versus-remote-sample race.
        fence(Ordering::SeqCst);
        self.sequence.fetch_add(1, Ordering::AcqRel);
        fence(Ordering::SeqCst);
    }

    /// Leaves watched kernel execution for the narrow idle/user window.
    ///
    /// # Safety
    /// The host must own this CPU's transitions, exclude ordinary IRQs and
    /// migration, and finish all protected accesses before this call. Nested
    /// NMIs must balance their own activation before resuming this instruction
    /// stream. Pair with `exit_quiescent` before ordinary kernel work resumes;
    /// no task switch or outstanding interrupt entry may span this window.
    pub unsafe fn enter_quiescent(&self) {
        debug_assert!(self.is_watching());
        self.cross_boundary();
    }

    /// Resumes watched execution before any ordinary kernel access.
    ///
    /// # Safety
    /// This must match `enter_quiescent` on the same CPU, under the same
    /// exclusion. All interrupt activations within that window must have exited.
    pub unsafe fn exit_quiescent(&self) {
        debug_assert!(!self.is_watching());
        self.cross_boundary();
    }

    /// Activates watching only if this entry interrupted a quiescent CPU.
    ///
    /// # Safety
    /// Call on the owning CPU before protected interrupt accesses, with ordinary
    /// IRQs and migration excluded. Nested entries must finish before the
    /// interrupted instruction stream resumes. Exit the returned entry exactly
    /// once, in nesting order, before returning to that stream. No scheduling,
    /// quiescent transition or abandonment may span an entry's lifetime.
    pub unsafe fn enter_interrupt(&self) -> InterruptEntry<'_> {
        let did_activate = !self.is_watching();
        if did_activate {
            self.cross_boundary();
        }
        #[cfg(any(test, unittest))]
        self.interrupt_depth.fetch_add(1, Ordering::Relaxed);
        InterruptEntry {
            cpu: self,
            did_activate,
            cpu_local: PhantomData,
        }
    }
}

impl Default for CpuWatching {
    fn default() -> Self {
        Self::new()
    }
}

/// Only the entry which activated watching may restore quiescence. Explicit
/// exit keeps the architecture return boundary visible instead of relying on
/// a Rust destructor that might run after scheduler work.
#[must_use = "interrupt entries must exit on their original CPU"]
pub struct InterruptEntry<'cpu> {
    cpu: &'cpu CpuWatching,
    did_activate: bool,
    cpu_local: PhantomData<*mut ()>,
}

impl InterruptEntry<'_> {
    /// Finishes this entry's protected work without changing an outer entry.
    ///
    /// # Safety
    /// Finish all protected accesses and nested entries first. Remain on the
    /// original CPU with the exclusion required by `enter_interrupt`; the
    /// interrupted stream must not resume until this exit has completed.
    pub unsafe fn exit(self) {
        debug_assert!(self.cpu.is_watching());
        #[cfg(any(test, unittest))]
        self.cpu.interrupt_depth.fetch_sub(1, Ordering::Relaxed);
        if self.did_activate {
            self.cpu.cross_boundary();
        }
    }
}

#[cfg(any(test, unittest))]
#[path = "../ut-tests/watching.rs"]
mod tests;

#[cfg(any(test, unittest))]
#[path = "../ut-tests/watching_gp.rs"]
mod tests_gp;

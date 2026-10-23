// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Static kernel lifecycle contract, separate from isolated core instances.

use core::marker::PhantomData;

use crate::{CpuWatching, TaskReader};

/// A retained current reader at a local deferred-cleanup boundary.
///
/// Construction is unsafe so the static interface cannot admit an arbitrary
/// reader through a safe call. This is a borrowed context, not a read guard.
pub struct ReaderCleanup<'a> {
    reader: &'a TaskReader,
    _local: PhantomData<*mut ()>,
}

impl<'a> ReaderCleanup<'a> {
    /// Certifies the caller-owned context for one immediate cleanup call.
    ///
    /// # Safety
    /// `reader` belongs to retained current task storage. Keep IRQs/preemption
    /// excluded in watched non-NMI context, with no RCU lock or concurrent
    /// enrollment/scope execution, until the cleanup call returns. Do not save
    /// this context for a later execution boundary.
    pub unsafe fn new(reader: &'a TaskReader) -> Self {
        Self {
            reader,
            _local: PhantomData,
        }
    }

    /// Borrows the reader whose cleanup context was established by the caller.
    pub fn reader(&self) -> &'a TaskReader {
        self.reader
    }
}

/// A current task retained through a scheduler observation on its CPU.
///
/// The scheduler may retain the same runner; an actual task switch is not
/// required. Active readers must still be enrolled before reporting the CPU.
pub struct SchedulerObservation<'a> {
    reader: &'a TaskReader,
    cpu: usize,
    _local: PhantomData<*mut ()>,
}

impl<'a> SchedulerObservation<'a> {
    /// Certifies one immediate current-task scheduler observation.
    ///
    /// # Safety
    /// `reader` belongs to the retained current task on discovered
    /// CPU `cpu`. Keep IRQs/preemption excluded in watched non-NMI scheduler
    /// context, with no RCU lock or concurrent enrollment/scope execution, until
    /// the observation returns. The boundary must end preceding implicit
    /// IRQ-off/no-preempt protection; exclusion acquired for the scheduler itself
    /// does not prevent observation. Retain task storage through terminal cleanup
    /// after its last switch. Do not save this context for a later boundary.
    pub unsafe fn new(reader: &'a TaskReader, cpu: usize) -> Self {
        Self {
            reader,
            cpu,
            _local: PhantomData,
        }
    }

    /// Borrows the current reader retained by the scheduling caller.
    pub fn reader(&self) -> &'a TaskReader {
        self.reader
    }

    /// Returns the discovered CPU performing this observation.
    pub fn cpu(&self) -> usize {
        self.cpu
    }
}

/// Connects scheduler and IRQ boundaries to the single kernel RCU owner.
///
/// The provider is linked permanently, including before its GP task starts.
/// It must preserve early reader enrollment and use initialized CPU identity.
/// This is not a consumer read-side or reclamation interface.
#[kiface::interface]
pub trait RcuContextIf {
    /// Borrows permanent watching storage for the CPU selected by this call.
    ///
    /// No locks, allocation, task lookup or scheduling are permitted, including
    /// in NMI. Preserve the caller's IRQ mask, never enable previously disabled
    /// IRQs, and do not change NMI exclusion. The caller pins transitions to this CPU;
    /// the reference itself neither pins execution nor authorizes a read scope.
    fn current_cpu_watching() -> &'static CpuWatching;

    /// Checks local deferred cleanup at a current-task preemption checkpoint.
    ///
    /// Requires initialized current task storage. The provider must reject NMI
    /// and unsafe cleanup contexts, suppress recursive guard-release checks and
    /// never schedule or report a CPU observation itself.
    fn check_reader_cleanup();

    /// Consumes local deferred cleanup without supplying a CPU observation.
    ///
    /// The caller must retain the context established by `ReaderCleanup::new`
    /// until this call returns. The provider must not schedule or wake inline.
    fn finish_reader_cleanup(context: ReaderCleanup<'_>);

    /// Requests a service pass after cleanup made an original node ready.
    ///
    /// This is a hint, not a period report. May run with scheduler locks held:
    /// never allocate, wake inline or schedule. NMI callers must be ignored;
    /// without a safe deferred carrier, retain periodic progress instead.
    fn request_completion_pass();

    /// Registers the current reader before reporting a scheduler observation.
    ///
    /// This also covers permitted wake probes that keep running the same task;
    /// it must not treat an active reader as finished merely because the CPU
    /// has reached this boundary.
    ///
    /// The caller retains the context established by `SchedulerObservation::new`.
    /// This call must not wake, allocate or schedule, even before GP startup.
    fn note_context_switch(context: SchedulerObservation<'_>);

    /// Exposes the permanent node only to controlled kernel boundary fixtures.
    #[cfg(unittest)]
    fn reader_node_for_tests(cpu: usize) -> &'static crate::ReaderNode;
}

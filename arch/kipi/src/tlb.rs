// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! TLB shootdown via IPI.
//!
//! When one CPU modifies page tables, other CPUs may hold stale TLB entries.
//! This module provides two IPI-based shootdown paths:
//!
//! 1. **Per-process flush** (`flush_process`): the initiator sends IPIs only to
//!    the CPUs that the current task has been scheduled on (tracked by
//!    `on_cpu_mask`).  Used for user page table modifications whose visibility
//!    is scoped to a single address space.
//!
//! 2. **All-CPU flush** (`flush_all_cpus`): the initiator broadcasts IPIs to
//!    **all** online CPUs via `for_each_present_logical_cpu()`.  Used for kernel
//!    page table modifications that are shared globally and must be visible on
//!    every CPU.
//!
//! Both paths share a zero-allocation shootdown mechanism: the initiator stores
//! the target virtual address in shared statics, sends IPIs to the target CPUs,
//! and spin-waits until every target CPU has performed the local flush and
//! acknowledged completion.
//!
//! The residency mask is only reset on a **full** TLB flush
//! (`flush_process(None)` or `flush_all_cpus(None)`), where every target CPU
//! has invalidated all entries.  For single-VA flushes the mask is preserved
//! so that CPUs are not prematurely removed while still holding valid TLB
//! entries for other virtual addresses.
//!
//! Implements the [`page_table::TlbFlushIf`] interface defined
//! in the `page_table` crate, breaking the circular dependency between
//! `page_table` and `kipi`.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kcpu_id_map::{KCpuMask, KCpuMaskExt, LogicalCpuId, for_each_present_logical_cpu};
use khal::{
    irq::{IPI_IRQ, TargetCpu},
    percpu::this_cpu_id,
};
use memaddr::VirtAddr;
use page_table::TlbFlushIf;

/// Gate: shootdown IPIs are only sent after all APs are running.
static ALL_CPUS_STARTED: AtomicBool = AtomicBool::new(false);

/// Virtual address to invalidate. Only valid when `SHOOTDOWN_FLUSH_ALL` is false.
static SHOOTDOWN_VADDR: AtomicUsize = AtomicUsize::new(0);

/// true → flush entire TLB; false → flush `SHOOTDOWN_VADDR` only.
static SHOOTDOWN_FLUSH_ALL: AtomicBool = AtomicBool::new(false);

/// Per-CPU flag set by the initiator to request a shootdown.
static PENDING: [AtomicBool; kbuild_config::CPU_NUM] =
    [const { AtomicBool::new(false) }; kbuild_config::CPU_NUM];

/// Per-CPU flag set by the target to signal completion.
static COMPLETED: [AtomicBool; kbuild_config::CPU_NUM] =
    [const { AtomicBool::new(false) }; kbuild_config::CPU_NUM];

/// Interface for querying per-task CPU residency, used to limit shootdown scope.
/// Implemented by the runtime layer which has access to `ktask`.
#[crate_interface::def_interface]
pub trait TaskCpuResidencyIf {
    /// Returns the set of CPUs the current task has been scheduled on.
    fn current_on_cpu_mask() -> KCpuMask;
    /// Resets the current task's residency mask to only the given CPU.
    fn reset_on_cpu_mask();
}

/// Mark that all secondary CPUs have entered the runtime.
///
/// Must be called exactly once from the primary CPU after
/// `start_secondary_cpus()` returns.
pub fn mark_all_cpus_started() {
    ALL_CPUS_STARTED.store(true, Ordering::Release);
}

struct TlbFlushImpl;

#[crate_interface::impl_interface]
impl TlbFlushIf for TlbFlushImpl {
    fn flush_process(vaddr: Option<VirtAddr>) {
        if !ALL_CPUS_STARTED.load(Ordering::Acquire) {
            return;
        }
        flush_remote(
            vaddr,
            crate_interface::call_interface!(TaskCpuResidencyIf::current_on_cpu_mask()),
        );
    }

    fn flush_all_cpus(vaddr: Option<VirtAddr>) {
        if !ALL_CPUS_STARTED.load(Ordering::Acquire) {
            return;
        }
        // Build a mask of all online CPUs — the kernel page table is
        // shared globally, so every CPU may hold stale entries.
        let mut all_mask = KCpuMask::new();
        for_each_present_logical_cpu(|_, cpu, _| {
            all_mask.set(cpu.as_usize(), true);
        });
        flush_remote(vaddr, all_mask);
    }
}

/// Shared shootdown logic: publish `vaddr`, send IPIs to every CPU set in
/// `target_mask` (except self), spin-wait for completion, and reset
/// residency masks on full flushes.
fn flush_remote(vaddr: Option<VirtAddr>, target_mask: KCpuMask) {
    let my_cpu = this_cpu_id();

    // Publish the target address.
    match vaddr {
        Some(va) => {
            SHOOTDOWN_FLUSH_ALL.store(false, Ordering::Relaxed);
            SHOOTDOWN_VADDR.store(va.as_usize(), Ordering::Relaxed);
        }
        None => {
            SHOOTDOWN_FLUSH_ALL.store(true, Ordering::Relaxed);
        }
    }

    let targets: [Option<LogicalCpuId>; kbuild_config::CPU_NUM] = {
        let mut buf = [None; kbuild_config::CPU_NUM];
        let mut idx = 0;
        for cpu in target_mask.iter_logical() {
            if cpu != my_cpu {
                buf[idx] = Some(cpu);
                idx += 1;
            }
        }
        buf
    };

    if targets[0].is_some() {
        for target in targets.iter().flatten() {
            let i = target.as_usize();
            COMPLETED[i].store(false, Ordering::Relaxed);
            PENDING[i].store(true, Ordering::Release);
        }

        core::sync::atomic::fence(Ordering::SeqCst);

        for target in targets.iter().flatten() {
            khal::irq::notify_cpu(IPI_IRQ, TargetCpu::Specific(target.as_usize()));
        }

        for target in targets.iter().flatten() {
            while !COMPLETED[target.as_usize()].load(Ordering::Acquire) {
                core::hint::spin_loop();
            }
        }
    }

    // Only reset the mask on a full TLB flush: after flush_all(None),
    // every CPU has invalidated *all* TLB entries for this address space,
    // so each thread only needs to track its current CPU.  For a single-VA
    // flush, the mask must be preserved — CPUs that were flushed for this
    // VA still hold valid entries for other VAs.
    if vaddr.is_none() {
        crate_interface::call_interface!(TaskCpuResidencyIf::reset_on_cpu_mask());
    }
}

/// Handle a pending TLB shootdown request on the current CPU.
///
/// Called from the IPI interrupt handler. If no shootdown is pending for
/// this CPU, returns immediately.
pub fn handle_shootdown() {
    let cpu: usize = this_cpu_id().into();
    if !PENDING[cpu].load(Ordering::Acquire) {
        return;
    }

    // Perform the local flush.
    if SHOOTDOWN_FLUSH_ALL.load(Ordering::Relaxed) {
        karch::flush_tlb(None);
    } else {
        let va = SHOOTDOWN_VADDR.load(Ordering::Relaxed);
        karch::flush_tlb(Some(VirtAddr::from(va)));
    }

    PENDING[cpu].store(false, Ordering::Relaxed);
    COMPLETED[cpu].store(true, Ordering::Release);
}

/// Test helper: trigger a TLB shootdown directly, bypassing the
/// `crate_interface` dispatch (which requires the defining crate's path).
#[cfg(unittest)]
pub fn trigger_flush_all(vaddr: Option<VirtAddr>) {
    <TlbFlushImpl as TlbFlushIf>::flush_all_cpus(vaddr);
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert, def_test};

    use super::*;

    #[def_test]
    fn test_handle_shootdown_no_pending() {
        let cpu: usize = this_cpu_id().into();
        PENDING[cpu].store(false, Ordering::Relaxed);
        COMPLETED[cpu].store(false, Ordering::Relaxed);
        handle_shootdown();
        assert!(!PENDING[cpu].load(Ordering::Acquire));
        assert!(!COMPLETED[cpu].load(Ordering::Acquire));
    }

    #[def_test]
    fn test_mark_all_cpus_started_gate() {
        // The gate is set by the kernel during boot; verify it stays true.
        mark_all_cpus_started();
        assert!(ALL_CPUS_STARTED.load(Ordering::Acquire));
    }

    #[def_test]
    fn test_shootdown_receive_path() {
        let cpu: usize = this_cpu_id().into();
        SHOOTDOWN_FLUSH_ALL.store(false, Ordering::Relaxed);
        SHOOTDOWN_VADDR.store(0xDEAD_BEEF, Ordering::Relaxed);
        COMPLETED[cpu].store(false, Ordering::Relaxed);
        PENDING[cpu].store(true, Ordering::Release);

        handle_shootdown();

        assert!(!PENDING[cpu].load(Ordering::Acquire));
        assert!(COMPLETED[cpu].load(Ordering::Acquire));
    }

    #[def_test]
    fn test_shootdown_full_flush() {
        let cpu: usize = this_cpu_id().into();
        SHOOTDOWN_FLUSH_ALL.store(true, Ordering::Relaxed);
        COMPLETED[cpu].store(false, Ordering::Relaxed);
        PENDING[cpu].store(true, Ordering::Release);

        handle_shootdown();

        assert!(!PENDING[cpu].load(Ordering::Acquire));
        assert!(COMPLETED[cpu].load(Ordering::Acquire));
    }

    #[def_test]
    fn test_double_handle_is_safe() {
        let cpu: usize = this_cpu_id().into();
        PENDING[cpu].store(true, Ordering::Release);
        handle_shootdown();
        handle_shootdown();
        assert!(COMPLETED[cpu].load(Ordering::Acquire));
    }

    /// Test A: Proves IPI reaches a remote CPU and handle_shootdown() executes.
    ///
    /// We manually set PENDING for a remote CPU, then use run_on_cpu() to send
    /// an IPI. The IPI handler calls handle_shootdown() first (which processes
    /// our PENDING, calls karch::flush_tlb, sets COMPLETED), then runs the
    /// callback.
    #[def_test]
    fn test_cross_cpu_shootdown_via_run_on_cpu() {
        let cpu_num = kbuild_config::CPU_NUM;
        if cpu_num >= 2 {
            let my_cpu = this_cpu_id();
            let remote_cpu = LogicalCpuId::new(if my_cpu == LogicalCpuId::new(0) { 1 } else { 0 });

            static REMOTE_DONE: AtomicBool = AtomicBool::new(false);
            REMOTE_DONE.store(false, Ordering::Relaxed);

            SHOOTDOWN_FLUSH_ALL.store(true, Ordering::Relaxed);
            COMPLETED[remote_cpu.as_usize()].store(false, Ordering::Relaxed);
            PENDING[remote_cpu.as_usize()].store(true, Ordering::Release);

            crate::run_on_cpu(remote_cpu, || {
                REMOTE_DONE.store(true, Ordering::Release);
            })
            .unwrap();

            while !COMPLETED[remote_cpu.as_usize()].load(Ordering::Acquire) {
                core::hint::spin_loop();
            }
            while !REMOTE_DONE.load(Ordering::Acquire) {
                core::hint::spin_loop();
            }

            assert!(COMPLETED[remote_cpu.as_usize()].load(Ordering::Acquire));
            assert!(!PENDING[remote_cpu.as_usize()].load(Ordering::Acquire));
            assert!(REMOTE_DONE.load(Ordering::Acquire));
        }
    }
}

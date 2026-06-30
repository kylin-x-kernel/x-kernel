// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! TLB shootdown via IPI.
//!
//! When one CPU modifies page tables, other CPUs may hold stale TLB entries.
//! This module provides two IPI-based shootdown paths:
//!
//! 1. **Per-address-space flush** (`flush_process_mask`): the initiator sends
//!    IPIs only to CPUs recorded in the target user address space's mm-owned
//!    residency mask. Used for user page table modifications whose visibility
//!    is scoped to a single address space.
//!
//! 2. **All-CPU flush** (`flush_all_cpus`): the initiator broadcasts IPIs to
//!    **all** online CPUs via `for_each_present_logical_cpu()`. Used for kernel
//!    page table modifications that are shared globally and must be visible on
//!    every CPU.
//!
//! Each initiator CPU owns one zero-allocation request slot. A request is
//! identified by the pair `(initiator_cpu, request_seq)`. Remote CPUs acknowledge
//! the highest sequence they have processed for each initiator, so concurrent
//! shootdowns from different CPUs cannot overwrite each other's completion
//! state.
//!
//! Residency maintenance is owned by the scheduler/context-switch path rather
//! than the flush path itself. Shootdown consumes a conservative residency
//! snapshot; it does not rebuild or reset residency after full or partial
//! flushes.
//!
//! Implements the [`page_table::TlbFlushIf`] interface defined in the
//! `page_table` crate, breaking the circular dependency between `page_table` and
//! `kipi`.

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use kcpu_id_map::{KCpuMask, KCpuMaskExt, LogicalCpuId, for_each_present_logical_cpu};
use khal::{
    irq::{IPI_IRQ, TargetCpu},
    percpu::this_cpu_id,
};
use kspin::NoPreempt;
use memaddr::VirtAddr;
use page_table::TlbFlushIf;

/// Gate: shootdown IPIs are only sent after all APs are running.
static ALL_CPUS_STARTED: AtomicBool = AtomicBool::new(false);

/// Monotonic sequence allocator per initiator CPU.
static REQUEST_SLOTS: [ShootdownRequestSlot; kbuild_config::CPU_NUM] =
    [const { ShootdownRequestSlot::new() }; kbuild_config::CPU_NUM];

/// Per-target fast path gate for `handle_shootdown()`.
///
/// Each initiator increments the epoch for every target CPU before sending a
/// TLB IPI. The target CPU snapshots its local epoch at IPI entry and skips the
/// O(CPU_NUM) slot scan when no epoch change has occurred since the last TLB
/// scan on that CPU.
static PENDING_EPOCH_BY_CPU: [AtomicU64; kbuild_config::CPU_NUM] =
    [const { AtomicU64::new(0) }; kbuild_config::CPU_NUM];

#[percpu::def_percpu]
static LAST_HANDLED_PENDING_EPOCH: u64 = 0;

const SHOOTDOWN_WARN_NS: u64 = 1_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RequestSeq(u64);

impl RequestSeq {
    const INITIAL: Self = Self(0);

    const fn get(self) -> u64 {
        self.0
    }

    const fn next_after(raw: u64) -> Self {
        if raw == u64::MAX {
            Self(1)
        } else {
            Self(raw + 1)
        }
    }
}

/// A request snapshot obtained by acquiring `published_seq`.
///
/// The constructor performs the acquire load on `published_seq`, so the
/// remaining request fields may be read with relaxed ordering through this
/// snapshot while still observing the state published before the matching
/// release store in `ShootdownRequestSlot::publish()`.
struct PublishedShootdownRequest<'a> {
    slot: &'a ShootdownRequestSlot,
    seq: RequestSeq,
}

impl<'a> PublishedShootdownRequest<'a> {
    fn seq(&self) -> RequestSeq {
        self.seq
    }

    fn targets_cpu(&self, cpu: usize) -> bool {
        self.slot.targeted_cpus[cpu].load(Ordering::Relaxed)
    }

    fn flush_vaddr(&self) -> Option<VirtAddr> {
        if self.slot.is_flush_all.load(Ordering::Relaxed) {
            None
        } else {
            Some(VirtAddr::from(
                self.slot.published_vaddr.load(Ordering::Relaxed),
            ))
        }
    }
}

struct ShootdownRequestSlot {
    next_seq: AtomicU64,
    published_seq: AtomicU64,
    published_vaddr: AtomicUsize,
    is_flush_all: AtomicBool,
    is_active: AtomicBool,
    needs_retry_full_flush: AtomicBool,
    targeted_cpus: [AtomicBool; kbuild_config::CPU_NUM],
    acked_seq_by_cpu: [AtomicU64; kbuild_config::CPU_NUM],
}

impl ShootdownRequestSlot {
    const fn new() -> Self {
        Self {
            next_seq: AtomicU64::new(RequestSeq::INITIAL.get()),
            published_seq: AtomicU64::new(RequestSeq::INITIAL.get()),
            published_vaddr: AtomicUsize::new(0),
            is_flush_all: AtomicBool::new(false),
            is_active: AtomicBool::new(false),
            needs_retry_full_flush: AtomicBool::new(false),
            targeted_cpus: [const { AtomicBool::new(false) }; kbuild_config::CPU_NUM],
            acked_seq_by_cpu: [const { AtomicU64::new(RequestSeq::INITIAL.get()) };
                kbuild_config::CPU_NUM],
        }
    }

    #[cfg(unittest)]
    fn reset_for_test(&self) {
        self.next_seq
            .store(RequestSeq::INITIAL.get(), Ordering::Relaxed);
        self.published_seq
            .store(RequestSeq::INITIAL.get(), Ordering::Relaxed);
        self.published_vaddr.store(0, Ordering::Relaxed);
        self.is_flush_all.store(false, Ordering::Relaxed);
        self.is_active.store(false, Ordering::Relaxed);
        self.needs_retry_full_flush.store(false, Ordering::Relaxed);
        for is_targeted in &self.targeted_cpus {
            is_targeted.store(false, Ordering::Relaxed);
        }
        for acked_seq in &self.acked_seq_by_cpu {
            acked_seq.store(RequestSeq::INITIAL.get(), Ordering::Relaxed);
        }
    }

    fn try_activate(&self) -> bool {
        self.is_active
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }

    fn deactivate(&self) {
        let was_active = self.is_active.swap(false, Ordering::AcqRel);
        debug_assert!(
            was_active,
            "deactivating an inactive shootdown request slot"
        );
    }

    fn request_retry_full_flush(&self) {
        self.needs_retry_full_flush.store(true, Ordering::Release);
    }

    fn take_retry_full_flush(&self) -> bool {
        self.needs_retry_full_flush.swap(false, Ordering::AcqRel)
    }

    fn allocate_seq(&self) -> RequestSeq {
        let previous = self
            .next_seq
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                Some(RequestSeq::next_after(current).get())
            })
            .expect("fetch_update closure always returns Some");
        let seq = RequestSeq::next_after(previous);
        debug_assert_ne!(seq, RequestSeq::INITIAL);
        seq
    }

    fn clear_targets(&self) {
        for is_targeted in &self.targeted_cpus {
            is_targeted.store(false, Ordering::Relaxed);
        }
    }

    fn mark_target(&self, target_cpu: LogicalCpuId) {
        self.targeted_cpus[target_cpu.as_usize()].store(true, Ordering::Relaxed);
    }

    fn publish(&self, request_seq: RequestSeq, vaddr: Option<VirtAddr>) {
        match vaddr {
            Some(va) => {
                self.is_flush_all.store(false, Ordering::Relaxed);
                self.published_vaddr.store(va.as_usize(), Ordering::Relaxed);
            }
            None => {
                self.is_flush_all.store(true, Ordering::Relaxed);
            }
        }
        self.published_seq
            .store(request_seq.get(), Ordering::Release);
    }

    fn load_published_request(&self) -> Option<PublishedShootdownRequest<'_>> {
        let seq = RequestSeq(self.published_seq.load(Ordering::Acquire));
        if seq == RequestSeq::INITIAL {
            return None;
        }
        Some(PublishedShootdownRequest { slot: self, seq })
    }

    fn is_acked_by(&self, cpu: usize, request_seq: RequestSeq) -> bool {
        self.acked_seq_by_cpu[cpu].load(Ordering::Acquire) == request_seq.get()
    }

    fn ack(&self, cpu: usize, request_seq: RequestSeq) {
        self.acked_seq_by_cpu[cpu].store(request_seq.get(), Ordering::Release);
    }

    fn targeted_snapshot(&self) -> [bool; kbuild_config::CPU_NUM] {
        core::array::from_fn(|cpu| self.targeted_cpus[cpu].load(Ordering::Relaxed))
    }

    fn acked_snapshot(&self) -> [u64; kbuild_config::CPU_NUM] {
        core::array::from_fn(|cpu| self.acked_seq_by_cpu[cpu].load(Ordering::Relaxed))
    }
}

/// Active borrow of the current CPU's shootdown request slot.
///
/// The guard disables preemption for the whole lifetime of the active slot so
/// that no other task scheduled onto the same CPU can re-enter `flush_remote()`
/// and reuse the same per-CPU slot before the current shootdown finishes.
///
/// Important: this guard must stay scoped to the request-slot publish/send/wait
/// window only. It must be dropped before any path that can block or sleep.
struct ActiveShootdownSlot<'a> {
    _no_preempt: NoPreempt,
    slot: &'a ShootdownRequestSlot,
    initiator: usize,
}

impl<'a> ActiveShootdownSlot<'a> {
    fn try_acquire_current() -> Option<Self> {
        let no_preempt = NoPreempt::new();
        let initiator = this_cpu_id().as_usize();
        let slot = &REQUEST_SLOTS[initiator];
        if !slot.try_activate() {
            return None;
        }
        Some(Self {
            _no_preempt: no_preempt,
            slot,
            initiator,
        })
    }

    fn initiator(&self) -> usize {
        self.initiator
    }

    fn allocate_seq(&self) -> RequestSeq {
        self.slot.allocate_seq()
    }

    fn clear_targets(&self) {
        self.slot.clear_targets();
    }

    fn mark_target(&self, target_cpu: LogicalCpuId) {
        self.slot.mark_target(target_cpu);
    }

    fn publish(&self, request_seq: RequestSeq, vaddr: Option<VirtAddr>) {
        self.slot.publish(request_seq, vaddr);
    }

    fn is_acked_by(&self, cpu: usize, request_seq: RequestSeq) -> bool {
        self.slot.is_acked_by(cpu, request_seq)
    }

    fn targeted_snapshot(&self) -> [bool; kbuild_config::CPU_NUM] {
        self.slot.targeted_snapshot()
    }

    fn acked_snapshot(&self) -> [u64; kbuild_config::CPU_NUM] {
        self.slot.acked_snapshot()
    }

    fn take_retry_full_flush(&self) -> bool {
        self.slot.take_retry_full_flush()
    }
}

impl Drop for ActiveShootdownSlot<'_> {
    fn drop(&mut self) {
        self.slot.deactivate();
    }
}

/// Mark that all secondary CPUs have entered the runtime.
///
/// Must be called exactly once from the primary CPU after
/// `start_secondary_cpus()` returns. Before this point `kipi` suppresses TLB
/// shootdown IPIs because not every target CPU is guaranteed to be able to
/// receive and acknowledge them.
pub fn mark_all_cpus_started() {
    ALL_CPUS_STARTED.store(true, Ordering::Release);
}

struct TlbFlushImpl;

#[crate_interface::impl_interface]
impl TlbFlushIf for TlbFlushImpl {
    fn flush_process_mask(vaddr: Option<VirtAddr>, target_mask: KCpuMask) {
        if !ALL_CPUS_STARTED.load(Ordering::Acquire) {
            return;
        }
        flush_remote(vaddr, target_mask);
    }

    fn flush_all_cpus(vaddr: Option<VirtAddr>) {
        if !ALL_CPUS_STARTED.load(Ordering::Acquire) {
            return;
        }

        flush_remote(vaddr, all_present_cpu_mask());
    }
}

fn all_present_cpu_mask() -> KCpuMask {
    let mut all_mask = KCpuMask::new();
    for_each_present_logical_cpu(|_, cpu, _| {
        all_mask.set(cpu.as_usize(), true);
    });
    all_mask
}

#[inline]
fn note_pending_for_cpu(target_cpu: LogicalCpuId) {
    PENDING_EPOCH_BY_CPU[target_cpu.as_usize()].fetch_add(1, Ordering::Release);
}

#[inline]
fn current_pending_epoch(cpu: usize) -> u64 {
    PENDING_EPOCH_BY_CPU[cpu].load(Ordering::Acquire)
}

#[inline]
fn last_handled_pending_epoch() -> u64 {
    // SAFETY: this reads the current CPU's local per-CPU slot only.
    unsafe { LAST_HANDLED_PENDING_EPOCH.read_current_raw() }
}

#[inline]
fn set_last_handled_pending_epoch(epoch: u64) {
    // SAFETY: this writes the current CPU's local per-CPU slot only.
    unsafe { LAST_HANDLED_PENDING_EPOCH.write_current_raw(epoch) }
}

/// Shared shootdown logic: publish `vaddr`, send IPIs to every CPU set in
/// `target_mask` (except self), and spin-wait for acknowledgement of this
/// exact request sequence.
fn flush_remote(vaddr: Option<VirtAddr>, target_mask: KCpuMask) {
    let should_retry = {
        let Some(active_slot) = ActiveShootdownSlot::try_acquire_current() else {
            let initiator = this_cpu_id().as_usize();
            REQUEST_SLOTS[initiator].request_retry_full_flush();
            return;
        };
        let my_cpu = this_cpu_id();

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

        let request_seq = active_slot.allocate_seq();

        active_slot.clear_targets();
        for target in targets.iter().flatten() {
            active_slot.mark_target(*target);
        }
        active_slot.publish(request_seq, vaddr);

        if targets[0].is_some() {
            for target in targets.iter().flatten() {
                note_pending_for_cpu(*target);
            }
            // `notify_cpu()` is responsible for publish-before-notify ordering
            // on each architecture, so the target cannot observe the IPI
            // before this request becomes visible.
            for target in targets.iter().flatten() {
                khal::irq::notify_cpu(IPI_IRQ, TargetCpu::Specific(target.as_usize()));
            }

            let start_ns = khal::time::monotonic_time_nanos();
            let mut warned = false;
            for target in targets.iter().flatten() {
                while !active_slot.is_acked_by(target.as_usize(), request_seq) {
                    let elapsed_ns = khal::time::monotonic_time_nanos().wrapping_sub(start_ns);
                    if !warned && elapsed_ns >= SHOOTDOWN_WARN_NS {
                        warned = true;
                        warn!(
                            "tlb shootdown wait: initiator_cpu={} stuck_on_target_cpu={} \
                             flush_all={} vaddr={:?} request_seq={} targeted={:?} acked={:?}",
                            active_slot.initiator(),
                            target.as_usize(),
                            vaddr.is_none(),
                            vaddr,
                            request_seq.get(),
                            active_slot.targeted_snapshot(),
                            active_slot.acked_snapshot()
                        );
                    }
                    core::hint::spin_loop();
                }
            }
        }

        active_slot.take_retry_full_flush()
    };

    if should_retry {
        flush_remote(None, all_present_cpu_mask());
    }
}

/// Handle any pending TLB shootdown requests targeting the current CPU.
///
/// Called from the shared IPI interrupt handler.
///
/// The fast path checks this CPU's pending epoch first and returns immediately
/// when no TLB request has targeted the CPU since the last scan, so generic
/// IPI callbacks do not pay the full request-slot walk cost.
pub fn handle_shootdown() {
    let cpu: usize = this_cpu_id().into();
    let pending_epoch = current_pending_epoch(cpu);
    if pending_epoch == last_handled_pending_epoch() {
        return;
    }

    for request_slot in &REQUEST_SLOTS {
        let Some(request) = request_slot.load_published_request() else {
            continue;
        };
        if request_slot.is_acked_by(cpu, request.seq()) {
            continue;
        }
        if !request.targets_cpu(cpu) {
            continue;
        }

        if let Some(vaddr) = request.flush_vaddr() {
            karch::flush_tlb(Some(vaddr));
        } else {
            karch::flush_tlb(None);
        }

        request_slot.ack(cpu, request.seq());
    }

    // Store the entry snapshot, not a post-scan re-read. If a new request
    // arrives during the scan, it will raise the epoch again and force another
    // scan on the next IPI instead of being accidentally absorbed here.
    set_last_handled_pending_epoch(pending_epoch);
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

    struct AllCpusStartedGuard(bool);

    impl AllCpusStartedGuard {
        fn save() -> Self {
            Self(ALL_CPUS_STARTED.load(Ordering::Acquire))
        }
    }

    impl Drop for AllCpusStartedGuard {
        fn drop(&mut self) {
            ALL_CPUS_STARTED.store(self.0, Ordering::Release);
        }
    }

    fn reset_request_state() {
        for pending_epoch in &PENDING_EPOCH_BY_CPU {
            pending_epoch.store(0, Ordering::Relaxed);
        }
        crate::run_on_each_cpu(|| {
            set_last_handled_pending_epoch(0);
        })
        .expect("reset per-cpu handled epochs");
        for request_slot in &REQUEST_SLOTS {
            request_slot.reset_for_test();
        }
    }

    fn publish_test_request(
        slot: &ShootdownRequestSlot,
        request_seq: RequestSeq,
        target_cpu: LogicalCpuId,
        vaddr: Option<VirtAddr>,
    ) {
        slot.mark_target(target_cpu);
        slot.publish(request_seq, vaddr);
        note_pending_for_cpu(target_cpu);
        slot.ack(target_cpu.as_usize(), RequestSeq::INITIAL);
    }

    #[def_test(serial)]
    fn test_handle_shootdown_no_pending() {
        reset_request_state();
        let cpu: usize = this_cpu_id().into();
        let request_slot = &REQUEST_SLOTS[cpu];
        request_slot.ack(cpu, RequestSeq::INITIAL);
        handle_shootdown();
        assert!(request_slot.is_acked_by(cpu, RequestSeq::INITIAL));
    }

    #[def_test(serial)]
    fn test_mark_all_cpus_started_gate() {
        let _all_cpus_started_guard = AllCpusStartedGuard::save();
        reset_request_state();
        ALL_CPUS_STARTED.store(false, Ordering::Relaxed);
        mark_all_cpus_started();
        assert!(ALL_CPUS_STARTED.load(Ordering::Acquire));
    }

    #[def_test(serial)]
    fn test_shootdown_receive_path() {
        reset_request_state();
        let cpu: usize = this_cpu_id().into();
        let request_slot = &REQUEST_SLOTS[cpu];
        let request_seq = RequestSeq(1);
        publish_test_request(
            request_slot,
            request_seq,
            this_cpu_id(),
            Some(VirtAddr::from(0xDEAD_BEEF)),
        );

        handle_shootdown();

        assert!(request_slot.is_acked_by(cpu, request_seq));
    }

    #[def_test(serial)]
    fn test_shootdown_full_flush() {
        reset_request_state();
        let cpu: usize = this_cpu_id().into();
        let request_slot = &REQUEST_SLOTS[cpu];
        let request_seq = RequestSeq(1);
        publish_test_request(request_slot, request_seq, this_cpu_id(), None);

        handle_shootdown();

        assert!(request_slot.is_acked_by(cpu, request_seq));
    }

    #[def_test(serial)]
    fn test_double_handle_is_safe() {
        reset_request_state();
        let cpu: usize = this_cpu_id().into();
        let request_slot = &REQUEST_SLOTS[cpu];
        let request_seq = RequestSeq(1);
        publish_test_request(request_slot, request_seq, this_cpu_id(), None);
        handle_shootdown();
        handle_shootdown();
        assert!(request_slot.is_acked_by(cpu, request_seq));
    }

    #[def_test(serial)]
    fn test_pending_epoch_fast_path_gates_empty_ipi() {
        reset_request_state();
        let cpu: usize = this_cpu_id().into();
        assert_eq!(current_pending_epoch(cpu), 0);
        assert_eq!(last_handled_pending_epoch(), 0);
        handle_shootdown();
        assert_eq!(last_handled_pending_epoch(), 0);
    }

    /// Proves IPI reaches a remote CPU and `handle_shootdown()` executes.
    #[def_test(serial)]
    fn test_cross_cpu_shootdown_via_run_on_cpu() {
        reset_request_state();
        let cpu_num = kbuild_config::CPU_NUM;
        if cpu_num >= 2 {
            let my_cpu = this_cpu_id();
            let mut remote_cpu = None;
            for_each_present_logical_cpu(|_, cpu_id, _| {
                if remote_cpu.is_some() || cpu_id == my_cpu {
                    return;
                }
                if crate::IPI_QUEUE_READY[cpu_id.as_usize()].load(Ordering::Acquire) {
                    remote_cpu = Some(cpu_id);
                }
            });
            let Some(remote_cpu) = remote_cpu else {
                return unittest::TestResult::Ok;
            };

            static REMOTE_DONE: AtomicBool = AtomicBool::new(false);
            REMOTE_DONE.store(false, Ordering::Relaxed);

            let request_slot = &REQUEST_SLOTS[my_cpu.as_usize()];
            let request_seq = RequestSeq(1);
            publish_test_request(request_slot, request_seq, remote_cpu, None);

            crate::run_on_cpu(remote_cpu, || {
                REMOTE_DONE.store(true, Ordering::Release);
            })
            .unwrap();

            while !request_slot.is_acked_by(remote_cpu.as_usize(), request_seq) {
                core::hint::spin_loop();
            }
            // The callback runs from `ipi_handler()` after its TLB shootdown
            // pass, so observing `REMOTE_DONE` also implies the remote CPU has
            // already had a chance to acknowledge `request_slot`.
            while !REMOTE_DONE.load(Ordering::Acquire) {
                core::hint::spin_loop();
            }

            assert!(request_slot.is_acked_by(remote_cpu.as_usize(), request_seq));
            assert!(REMOTE_DONE.load(Ordering::Acquire));
        }
    }

    /// Proves distinct initiator slots keep independent target and
    /// acknowledgement state.
    ///
    /// The live remote-handler path is covered by
    /// `test_cross_cpu_shootdown_via_run_on_cpu()`. This test stays purely
    /// local so it does not race with background kernel activity that may
    /// legitimately reuse another CPU's global request slot during full-system
    /// unit-test runs.
    #[def_test(serial)]
    fn test_dual_initiator_requests_are_isolated() {
        let cpu_num = kbuild_config::CPU_NUM;
        if cpu_num < 2 {
            return unittest::TestResult::Ok;
        }

        let first_cpu = LogicalCpuId::new(0);
        let second_cpu = LogicalCpuId::new(1);
        let first_slot = ShootdownRequestSlot::new();
        let second_slot = ShootdownRequestSlot::new();

        let first_seq = RequestSeq(1);
        let second_seq = RequestSeq(2);

        first_slot.mark_target(second_cpu);
        second_slot.mark_target(first_cpu);
        first_slot.publish(first_seq, Some(VirtAddr::from(0x1000)));
        second_slot.publish(second_seq, Some(VirtAddr::from(0x2000)));

        first_slot.ack(second_cpu.as_usize(), first_seq);
        second_slot.ack(first_cpu.as_usize(), second_seq);

        assert!(first_slot.is_acked_by(second_cpu.as_usize(), first_seq));
        assert!(second_slot.is_acked_by(first_cpu.as_usize(), second_seq));
        assert!(!first_slot.is_acked_by(first_cpu.as_usize(), first_seq));
        assert!(!second_slot.is_acked_by(second_cpu.as_usize(), second_seq));
        assert_eq!(first_slot.targeted_snapshot()[second_cpu.as_usize()], true);
        assert_eq!(second_slot.targeted_snapshot()[first_cpu.as_usize()], true);
        assert_eq!(first_slot.targeted_snapshot()[first_cpu.as_usize()], false);
        assert_eq!(
            second_slot.targeted_snapshot()[second_cpu.as_usize()],
            false
        );
    }
}

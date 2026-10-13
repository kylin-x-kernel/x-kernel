// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

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
    let cpu_num = kcpu_id_map::nr_cpus();
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

/// Proves distinct initiator slots keep independent target and acknowledgement state.
///
/// The live remote-handler path is covered by
/// `test_cross_cpu_shootdown_via_run_on_cpu()`. This test stays purely local so it
/// does not race with background kernel activity that may legitimately reuse another
/// CPU's global request slot during full-system unit-test runs.
#[def_test(serial)]
fn test_dual_initiator_requests_are_isolated() {
    let cpu_num = kcpu_id_map::nr_cpus();
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

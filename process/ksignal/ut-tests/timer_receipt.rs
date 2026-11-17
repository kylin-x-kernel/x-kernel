// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Weak;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use unittest::{assert, assert_eq, def_test};

use super::*;
use crate::{TimerSignalOrigin, api::SignalActions};

static COMPLETIONS: AtomicUsize = AtomicUsize::new(0);

struct RecordOrigin {
    completed: AtomicUsize,
    available: AtomicUsize,
    is_completion_valid: AtomicBool,
    is_availability_valid: AtomicBool,
    record: SpinNoIrq<Option<crate::PreparedTimerRecord>>,
}

impl TimerSignalOrigin for RecordOrigin {
    fn complete(&self, timer_id: i32, _: u64) -> crate::SignalDequeueAction {
        self.is_completion_valid.store(
            timer_id == 17 && prepare_record(self, Signo::SIGRTMIN, 99).is_none(),
            Ordering::Relaxed,
        );
        self.completed.fetch_add(1, Ordering::Relaxed);
        crate::SignalDequeueAction::Deliver
    }

    fn record_available(&self, timer_id: i32, record: &crate::PreparedTimerRecord) {
        let is_valid = timer_id == 17
            && self.record.lock().as_ref().unwrap().same_storage(record)
            && !crate::process_one_ready_timer_record();
        self.is_availability_valid
            .fetch_and(is_valid, Ordering::Relaxed);
        self.available.fetch_add(1, Ordering::Relaxed);
    }
}

fn record_origin() -> Arc<RecordOrigin> {
    let origin = Arc::new(RecordOrigin {
        completed: AtomicUsize::new(0),
        available: AtomicUsize::new(0),
        is_completion_valid: AtomicBool::new(false),
        is_availability_valid: AtomicBool::new(true),
        record: SpinNoIrq::new(None),
    });
    let weak = Arc::downgrade(&origin);
    let record = crate::PreparedTimerRecord::new(weak).unwrap();
    *origin.record.lock() = Some(record);
    origin
}

fn prepare_record(
    origin: &RecordOrigin,
    signo: Signo,
    sequence: u64,
) -> Option<SignalNotification> {
    origin.record.lock().as_ref().unwrap().prepare(
        signo,
        17,
        0,
        0,
        sequence,
        false,
    )
}

#[def_test]
fn private_realtime_owner_failure_preserves_projection_and_fatal_epoch() {
    for is_blocked in [false, true] {
        let process = Arc::new(ProcessSignalManager::new(SignalActions::default(), 0));
        let thread = ThreadSignalManager::new(1, process.clone()).unwrap();
        let mut mask = SignalSet::default();
        mask.add(Signo::SIGRTMIN);
        if is_blocked {
            thread.set_blocked(mask);
        }
        let before = process.fatal_signal_snapshot();
        let (should_wake, outcome) = ThreadSignalManager::send_to_slot_with_allocator(
            &process,
            thread.slot,
            SignalInfo::new_user(Signo::SIGRTMIN, linux_raw_sys::general::SI_TKILL, 123).into(),
            false,
            |_| None,
        );
        assert!(!should_wake);
        assert_eq!(outcome.admission_result(), Err(kerrno::KError::WouldBlock));
        assert_eq!(outcome.wake_tid(), None);
        assert!(thread.pending().is_empty());
        assert!(!thread.may_have_signal());
        assert_eq!(process.fatal_signal_snapshot(), before);
        assert_eq!(
            thread
                .send_signal(SignalInfo::new_user(
                    Signo::SIGRTMIN,
                    linux_raw_sys::general::SI_TKILL,
                    123,
                ))
                .unwrap(),
            !is_blocked
        );
        assert_eq!(thread.may_have_signal(), !is_blocked);
        assert_eq!(process.fatal_signal_snapshot() != before, !is_blocked);
        assert_eq!(
            thread.dequeue_signal(&mask).unwrap().code(),
            linux_raw_sys::general::SI_TKILL
        );
        assert!(thread.pending().is_empty());
    }
}

#[def_test(serial)]
fn standard_fallback_never_creates_timer_authority_or_an_extra_occurrence() {
    let thread = setup();
    assert!(
        thread
            .send_signal(SignalInfo::new_kernel(Signo::SIGKILL))
            .unwrap()
    );
    assert!(thread.may_have_signal());
    assert_eq!(
        thread.take_signal().unwrap().sig.code(),
        linux_raw_sys::general::SI_USER as i32
    );
    assert!(!thread.may_have_signal());
    let origin = record_origin();
    let mut pending = crate::PendingSignals::default();
    let signo = Signo::SIGUSR1;
    let mut mask = SignalSet::default();
    mask.add(signo);
    let forged = SignalInfo::new_timer(signo, 17, 99, 42, 23);
    assert!(
        pending
            .put_notification_with_allocator(forged.into(), |_| None)
            .unwrap()
            .is_none()
    );
    let (info, receipt) = pending.dequeue_notification(&mask).unwrap().into_parts();
    assert_eq!(info.code(), linux_raw_sys::general::SI_USER as i32);
    assert!(info.timer_id().is_none());
    assert!(receipt.is_none());
    assert_eq!(origin.completed.load(Ordering::Relaxed), 0);

    assert!(
        pending
            .put_notification_with_allocator(SignalInfo::new_kernel(signo).into(), |_| None)
            .unwrap()
            .is_none()
    );
    let allocations = core::cell::Cell::new(0);
    assert!(
        pending
            .put_notification_with_allocator(prepare_record(&origin, signo, 23).unwrap(), |_| {
                allocations.set(allocations.get() + 1);
                None
            })
            .unwrap()
            .is_none()
    );
    assert_eq!(allocations.get(), 0);
    let (_, receipt) = pending.dequeue_notification(&mask).unwrap().into_parts();
    assert!(matches!(
        receipt.unwrap().complete(),
        crate::SignalDequeueAction::Deliver
    ));
    assert_eq!(origin.completed.load(Ordering::Relaxed), 1);
    assert!(pending.set.is_empty());
    assert!(pending.dequeue_notification(&mask).is_none());
    drop(MANAGER.lock().take());
}

#[def_test(serial)]
fn cancelling_ignored_timer_keeps_an_unrelated_bit_only_signal() {
    let origin = record_origin();
    let record = origin.record.lock().as_ref().unwrap().clone();
    let mut pending = crate::PendingSignals::default();
    let signo = Signo::SIGUSR1;
    let mut mask = SignalSet::default();
    mask.add(signo);
    pending.retain_ignored(
        record
            .prepare(
                signo,
                17,
                0,
                0,
                23,
                true,
            )
            .unwrap(),
    );
    assert!(
        pending
            .put_notification_with_allocator(SignalInfo::new_kernel(signo).into(), |_| None)
            .unwrap()
            .is_none()
    );
    record.invalidate_through(23);
    let detached = pending.cancel_timer(&record, 23);
    assert!(detached.is_some());
    drop(detached);
    assert!(pending.set.has(signo));
    assert_eq!(
        pending.dequeue_signal(&mask).unwrap().code(),
        linux_raw_sys::general::SI_USER as i32
    );
    assert!(pending.set.is_empty());
    assert_eq!(origin.completed.load(Ordering::Relaxed), 0);
    assert!(prepare_record(&origin, signo, 24).is_some());
}

#[def_test(serial)]
fn ignored_periodic_records_replay_but_one_shots_do_not() {
    for private in [false, true] {
        let thread = setup();
        let target = if private {
            thread.timer_target().unwrap()
        } else {
            crate::TimerSignalTarget::process(Arc::downgrade(thread.process()))
        };
        let signo = Signo::SIGUSR1;
        thread
            .proc
            .set_signal_action(
                signo,
                SignalAction {
                    disposition: SignalDisposition::Ignore,
                    ..Default::default()
                },
            )
            .unwrap();
        let periodic = record_origin();
        let one_shot = record_origin();
        let notification = periodic
            .record
            .lock()
            .as_ref()
            .unwrap()
            .prepare(
                signo,
                17,
                0,
                0,
                23,
                true,
            )
            .unwrap();
        assert!(matches!(
            target.send(notification),
            TimerSignalSendOutcome::Retained
        ));
        drop(returned_notification(
            target.send(prepare_record(&one_shot, signo, 23).unwrap()),
            TimerSignalRejection::Ignored,
        ));
        assert!(thread.pending().is_empty());
        assert_eq!(target.pending_wake_tid(signo), None);
        assert!(prepare_record(&periodic, signo, 24).is_none());
        assert!(prepare_record(&one_shot, signo, 24).is_some());
        thread
            .proc
            .set_signal_action(signo, SignalAction::default())
            .unwrap();
        assert_eq!(target.pending_wake_tid(signo), Some(71));
        assert!(thread.take_signal().is_some());
        assert!(thread.take_signal().is_none());
        assert_eq!(periodic.completed.load(Ordering::Relaxed), 1);
        assert_eq!(one_shot.completed.load(Ordering::Relaxed), 0);
        thread
            .proc
            .set_signal_action(
                signo,
                SignalAction {
                    disposition: SignalDisposition::Ignore,
                    ..Default::default()
                },
            )
            .unwrap();
        let notification = periodic
            .record
            .lock()
            .as_ref()
            .unwrap()
            .prepare(
                signo,
                17,
                0,
                0,
                25,
                true,
            )
            .unwrap();
        let _ = target.send(notification);
        drop(thread);
        drop(MANAGER.lock().take());
        assert!(prepare_record(&periodic, signo, 26).is_some());
        assert_eq!(periodic.completed.load(Ordering::Relaxed), 1);
    }
}

#[def_test(serial)]
fn ignore_flushes_blocked_owners_and_same_value_reinstallation() {
    for private in [false, true] {
        let thread = setup();
        let target = if private {
            thread.timer_target().unwrap()
        } else {
            crate::TimerSignalTarget::process(Arc::downgrade(thread.process()))
        };
        let signo = Signo::SIGUSR1;
        let mut mask = SignalSet::default();
        mask.add(signo);
        thread.set_blocked(mask);
        let periodic = record_origin();
        let one_shot = record_origin();
        let _ = target.send(SignalInfo::new_kernel(signo).into());
        let notification = periodic
            .record
            .lock()
            .as_ref()
            .unwrap()
            .prepare(
                signo,
                17,
                0,
                0,
                23,
                true,
            )
            .unwrap();
        let _ = target.send(notification);
        let _ = target.send(prepare_record(&one_shot, signo, 23).unwrap());
        let ignore = SignalAction {
            disposition: SignalDisposition::Ignore,
            ..Default::default()
        };
        thread
            .proc
            .set_signal_action(signo, ignore.clone())
            .unwrap();
        assert!(thread.pending().is_empty());
        assert!(prepare_record(&periodic, signo, 24).is_none());
        assert!(prepare_record(&one_shot, signo, 24).is_some());
        let _ = target.send(SignalInfo::new_kernel(signo).into());
        assert!(thread.pending().has(signo));
        thread.proc.set_signal_action(signo, ignore).unwrap();
        assert!(thread.pending().is_empty());
        thread
            .proc
            .set_signal_action(signo, SignalAction::default())
            .unwrap();
        assert!(thread.dequeue_signal(&mask).is_some());
        assert!(thread.dequeue_signal(&mask).is_none());
        assert_eq!(periodic.completed.load(Ordering::Relaxed), 1);
        assert_eq!(one_shot.completed.load(Ordering::Relaxed), 0);
        drop(MANAGER.lock().take());
    }
}

#[def_test(serial)]
fn ignored_periodic_mask_replay_does_not_acknowledge_handler_discard() {
    for private in [false, true] {
        let thread = setup();
        let target = if private {
            thread.timer_target().unwrap()
        } else {
            crate::TimerSignalTarget::process(Arc::downgrade(thread.process()))
        };
        let signo = Signo::SIGUSR1;
        let mut mask = SignalSet::default();
        mask.add(signo);
        thread
            .proc
            .set_signal_action(
                signo,
                SignalAction {
                    disposition: SignalDisposition::Ignore,
                    ..Default::default()
                },
            )
            .unwrap();
        let origin = record_origin();
        let notification = origin
            .record
            .lock()
            .as_ref()
            .unwrap()
            .prepare(
                signo,
                17,
                0,
                0,
                23,
                true,
            )
            .unwrap();
        let _ = target.send(notification);
        thread.set_blocked(mask);
        assert!(thread.pending().has(signo));
        let unblocked_peer = ThreadSignalManager::new(72, thread.proc.clone()).unwrap();
        assert_eq!(target.pending_wake_tid(signo), None);
        thread.set_blocked(SignalSet::default());
        assert_eq!(target.pending_wake_tid(signo), None);
        assert!(thread.take_signal().is_none());
        assert!(thread.pending().is_empty());
        assert_eq!(origin.completed.load(Ordering::Relaxed), 0);
        assert!(prepare_record(&origin, signo, 24).is_none());
        thread.set_blocked(mask);
        assert!(thread.dequeue_signal(&mask).is_some());
        assert_eq!(origin.completed.load(Ordering::Relaxed), 1);
        drop(unblocked_peer);
        drop(MANAGER.lock().take());
    }
}

#[def_test(serial)]
fn ignored_cancellation_preserves_a_later_generation() {
    for private in [false, true] {
        let thread = setup();
        let target = if private {
            thread.timer_target().unwrap()
        } else {
            crate::TimerSignalTarget::process(Arc::downgrade(thread.process()))
        };
        let signo = Signo::SIGUSR1;
        thread
            .proc
            .set_signal_action(
                signo,
                SignalAction {
                    disposition: SignalDisposition::Ignore,
                    ..Default::default()
                },
            )
            .unwrap();
        let origin = record_origin();
        let record = origin.record.lock().as_ref().unwrap().clone();
        let _ = target.send(
            record
                .prepare(
                    signo,
                    17,
                    0,
                    0,
                    23,
                    true,
                )
                .unwrap(),
        );
        let delayed = record.invalidate(&target, 23);
        drop(record.invalidate(&target, 23));
        let _ = target.send(
            record
                .prepare(
                    signo,
                    17,
                    0,
                    0,
                    24,
                    true,
                )
                .unwrap(),
        );
        drop(delayed);
        thread
            .proc
            .set_signal_action(signo, SignalAction::default())
            .unwrap();
        assert_eq!(
            thread.take_signal().unwrap().sig.timer_signal_seq(),
            Some(24)
        );
        assert_eq!(origin.completed.load(Ordering::Relaxed), 1);
        drop(MANAGER.lock().take());
    }
}

#[def_test(serial)]
fn shared_actions_flush_only_the_calling_group() {
    let thread = setup();
    let peer = thread.proc.fork_shared().unwrap();
    let origin = record_origin();
    let signo = Signo::SIGUSR1;
    let notification = origin
        .record
        .lock()
        .as_ref()
        .unwrap()
        .prepare(
            signo,
            17,
            0,
            0,
            23,
            true,
        )
        .unwrap();
    let _ = thread.timer_target().unwrap().send(notification);
    peer.set_signal_action(
        signo,
        SignalAction {
            disposition: SignalDisposition::Ignore,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(thread.pending().has(signo));
    assert!(thread.take_signal().is_none());
    assert_eq!(origin.completed.load(Ordering::Relaxed), 0);
    peer.set_signal_action(signo, SignalAction::default())
        .unwrap();
    assert!(thread.pending().is_empty());
    thread
        .proc
        .set_signal_action(signo, SignalAction::default())
        .unwrap();
    assert!(thread.take_signal().is_some());
    assert_eq!(origin.completed.load(Ordering::Relaxed), 1);
    drop(MANAGER.lock().take());
}

#[def_test(serial)]
fn ignored_replay_discards_exited_targets_without_completion() {
    for private in [false, true] {
        let thread = setup();
        let target = if private {
            thread.timer_target().unwrap()
        } else {
            crate::TimerSignalTarget::process(Arc::downgrade(thread.process()))
        };
        let signo = Signo::SIGUSR1;
        thread
            .proc
            .set_signal_action(
                signo,
                SignalAction {
                    disposition: SignalDisposition::Ignore,
                    ..Default::default()
                },
            )
            .unwrap();
        let origin = record_origin();
        let notification = origin
            .record
            .lock()
            .as_ref()
            .unwrap()
            .prepare(
                signo,
                17,
                0,
                0,
                23,
                true,
            )
            .unwrap();
        let _ = target.send(notification);
        thread.exit();
        thread
            .proc
            .set_signal_action(signo, SignalAction::default())
            .unwrap();
        assert!(thread.pending().is_empty());
        assert_eq!(target.pending_wake_tid(signo), None);
        assert!(prepare_record(&origin, signo, 24).is_some());
        assert_eq!(origin.completed.load(Ordering::Relaxed), 0);
        drop(MANAGER.lock().take());
    }
}

#[def_test(serial)]
fn prepared_standard_timers_share_fifo_without_granting_abi_timer_authority() {
    for private in [false, true] {
        let thread = setup();
        let target = if private {
            thread.timer_target().unwrap()
        } else {
            crate::TimerSignalTarget::process(Arc::downgrade(thread.process()))
        };
        let signo = Signo::SIGUSR1;
        let mut mask = SignalSet::default();
        mask.add(signo);
        thread.set_blocked(mask);
        let first = record_origin();
        let second = record_origin();
        assert!(matches!(
            target.send(
                SignalInfo::new_timer(signo, 101, 0, 0, 0)
                    .into()
            ),
            TimerSignalSendOutcome::Queued { .. }
        ));
        assert!(matches!(
            target.send(prepare_record(&first, signo, 23).unwrap()),
            TimerSignalSendOutcome::Queued { .. }
        ));
        assert!(matches!(
            target.send(prepare_record(&second, signo, 24).unwrap()),
            TimerSignalSendOutcome::Queued { .. }
        ));
        assert!(prepare_record(&first, signo, 25).is_none());
        let forged =
            SignalInfo::new_timer(signo, 999, 0, 0, 23);
        drop(returned_notification(
            target.send(forged.into()),
            TimerSignalRejection::Coalesced,
        ));
        assert_eq!(thread.dequeue_signal(&mask).unwrap().timer_id(), Some(101));
        assert_eq!(
            thread.dequeue_signal(&mask).unwrap().timer_signal_seq(),
            Some(23)
        );
        assert_eq!(first.completed.load(Ordering::Relaxed), 1);
        drop(returned_notification(
            target.send(SignalInfo::new_kernel(signo).into()),
            TimerSignalRejection::Coalesced,
        ));
        assert!(matches!(
            target.send(prepare_record(&first, signo, 25).unwrap()),
            TimerSignalSendOutcome::Queued { .. }
        ));
        assert_eq!(
            thread.dequeue_signal(&mask).unwrap().timer_signal_seq(),
            Some(24)
        );
        assert_eq!(
            thread.dequeue_signal(&mask).unwrap().timer_signal_seq(),
            Some(25)
        );
        assert_eq!(first.completed.load(Ordering::Relaxed), 2);
        assert_eq!(second.completed.load(Ordering::Relaxed), 1);
        assert!(thread.dequeue_signal(&mask).is_none());
        assert!(matches!(
            target.send(SignalInfo::new_kernel(signo).into()),
            TimerSignalSendOutcome::Queued { .. }
        ));
        assert!(thread.dequeue_signal(&mask).is_some());
        drop(MANAGER.lock().take());
    }
}

#[def_test(serial)]
fn cancelling_standard_timer_preserves_other_timer_and_pending_bit() {
    for private in [false, true] {
        let thread = setup();
        let target = if private {
            thread.timer_target().unwrap()
        } else {
            crate::TimerSignalTarget::process(Arc::downgrade(thread.process()))
        };
        let first = record_origin();
        let second = record_origin();
        let mut mask = SignalSet::default();
        mask.add(Signo::SIGUSR1);
        thread.set_blocked(mask);
        assert!(matches!(
            target.send(prepare_record(&first, Signo::SIGUSR1, 1).unwrap()),
            TimerSignalSendOutcome::Queued { .. }
        ));
        assert!(matches!(
            target.send(prepare_record(&second, Signo::SIGUSR1, 1).unwrap()),
            TimerSignalSendOutcome::Queued { .. }
        ));
        let record = first.record.lock().as_ref().unwrap().clone();
        drop(record.invalidate(&target, 1));
        let pending = if private {
            thread.pending()
        } else {
            thread.process().pending()
        };
        assert!(pending.has(Signo::SIGUSR1));
        assert_eq!(first.completed.load(Ordering::Relaxed), 0);
        assert!(thread.dequeue_signal(&mask).is_some());
        assert_eq!(second.completed.load(Ordering::Relaxed), 1);
        assert!(thread.dequeue_signal(&mask).is_none());
        drop(prepare_record(&first, Signo::SIGUSR1, 2).unwrap());
        drop(MANAGER.lock().take());
    }
}

#[def_test(serial)]
fn cancellation_preserves_mixed_fifo_and_checks_captured_queue() {
    let thread = setup();
    let target = thread.timer_target().unwrap();
    let wrong_target = crate::TimerSignalTarget::process(Arc::downgrade(thread.process()));
    let origin = record_origin();
    let record = origin.record.lock().as_ref().unwrap().clone();
    let mut mask = SignalSet::default();
    mask.add(Signo::SIGRTMIN);
    thread.set_blocked(mask);
    let _ = thread
        .send_signal(SignalInfo::new_timer(
            Signo::SIGRTMIN,
            101,
            0,
            0,
            0,
        ))
        .unwrap();
    assert!(matches!(
        target.send(prepare_record(&origin, Signo::SIGRTMIN, 1).unwrap()),
        crate::TimerSignalSendOutcome::Queued { .. }
    ));
    let _ = thread
        .send_signal(SignalInfo::new_timer(
            Signo::SIGRTMIN,
            102,
            0,
            0,
            0,
        ))
        .unwrap();
    drop(record.invalidate(&wrong_target, 1));
    assert!(prepare_record(&origin, Signo::SIGRTMIN, 2).is_none());
    drop(record.invalidate(&target, 1));
    assert!(thread.pending().has(Signo::SIGRTMIN));
    assert_eq!(thread.dequeue_signal(&mask).unwrap().timer_id(), Some(101));
    assert_eq!(thread.dequeue_signal(&mask).unwrap().timer_id(), Some(102));
    assert!(!thread.pending().has(Signo::SIGRTMIN));
    assert_eq!(origin.completed.load(Ordering::Relaxed), 0);
    drop(prepare_record(&origin, Signo::SIGRTMIN, 2).unwrap());
    drop(MANAGER.lock().take());
}

#[def_test(serial)]
fn invalidation_cancels_exact_pending_owner_and_preserves_successor() {
    for signo in [Signo::SIGUSR1, Signo::SIGRTMIN] {
        for private in [false, true] {
            let thread = setup();
            let mut mask = SignalSet::default();
            mask.add(signo);
            thread.set_blocked(mask);
            let target = if private {
                thread.timer_target().unwrap()
            } else {
                crate::TimerSignalTarget::process(Arc::downgrade(thread.process()))
            };
            let origin = record_origin();
            let record = origin.record.lock().as_ref().unwrap().clone();
            assert!(matches!(
                target.send(prepare_record(&origin, signo, 1).unwrap()),
                crate::TimerSignalSendOutcome::Queued { .. }
            ));
            let cancellation = record.invalidate(&target, 1);
            let delayed_cancellation = record.invalidate(&target, 1);
            drop(cancellation);
            assert!(thread.dequeue_signal(&mask).is_none());
            assert_eq!(origin.completed.load(Ordering::Relaxed), 0);
            assert!(prepare_record(&origin, signo, 1).is_none());
            assert!(matches!(
                target.send(prepare_record(&origin, signo, 2).unwrap()),
                crate::TimerSignalSendOutcome::Queued { .. }
            ));
            drop(delayed_cancellation);
            assert!(thread.dequeue_signal(&mask).is_some());
            assert_eq!(origin.completed.load(Ordering::Relaxed), 1);
            assert!(thread.dequeue_signal(&mask).is_none());
            drop(MANAGER.lock().take());
        }
    }
}

#[def_test(serial)]
fn invalidation_rejects_inflight_lease_and_does_not_release_detached_owner() {
    let thread = setup();
    let target = thread.timer_target().unwrap();
    let origin = record_origin();
    let record = origin.record.lock().as_ref().unwrap().clone();
    let inflight = prepare_record(&origin, Signo::SIGRTMIN, 1).unwrap();
    drop(record.invalidate(&target, 1));
    let rejected = target.send(inflight);
    assert!(matches!(
        &rejected,
        crate::TimerSignalSendOutcome::Rejected {
            reason: crate::TimerSignalRejection::Invalidated,
            ..
        }
    ));
    assert!(prepare_record(&origin, Signo::SIGRTMIN, 2).is_none());
    drop(rejected);
    assert!(matches!(
        target.send(prepare_record(&origin, Signo::SIGRTMIN, 2).unwrap()),
        crate::TimerSignalSendOutcome::Queued { .. }
    ));
    let detached = thread
        .proc
        .siglock()
        .transact(|txn| txn.take(thread.slot))
        .unwrap();
    drop(record.invalidate(&target, 2));
    assert!(prepare_record(&origin, Signo::SIGRTMIN, 3).is_none());
    drop(detached);
    assert_eq!(origin.completed.load(Ordering::Relaxed), 0);
    drop(prepare_record(&origin, Signo::SIGRTMIN, 3).unwrap());
    drop(MANAGER.lock().take());
}

#[def_test(serial)]
fn prepared_record_mixed_fifo_keeps_lease_until_receipt_completion() {
    let origin = record_origin();
    let record = origin.record.lock().as_ref().unwrap().clone();
    assert!(record.same_storage(origin.record.lock().as_ref().unwrap()));
    let mut pending = crate::PendingSignals::default();
    let signo = Signo::SIGRTMIN;
    let mut mask = SignalSet::default();
    mask.add(signo);
    assert!(
        pending
            .put_signal(SignalInfo::new_timer(
                signo,
                1,
                0,
                0,
                1
            ))
            .unwrap()
    );
    assert!(
        pending
            .put_notification(prepare_record(&origin, signo, 23).unwrap())
            .unwrap()
            .is_none()
    );
    assert!(
        pending
            .put_signal(SignalInfo::new_timer(
                signo,
                2,
                0,
                0,
                2
            ))
            .unwrap()
    );
    assert!(prepare_record(&origin, signo, 24).is_none());
    assert_eq!(pending.dequeue_signal(&mask).unwrap().timer_id(), Some(1));
    let detached = pending.dequeue_notification(&mask).unwrap();
    assert!(prepare_record(&origin, signo, 24).is_none());
    let (info, receipt) = detached.into_parts();
    assert_eq!(info.timer_signal_seq(), Some(23));
    assert!(prepare_record(&origin, signo, 24).is_none());
    assert_eq!(
        receipt.unwrap().complete(),
        crate::SignalDequeueAction::Deliver
    );
    assert_eq!(origin.completed.load(Ordering::Relaxed), 1);
    assert!(origin.is_completion_valid.load(Ordering::Relaxed));
    assert_eq!(pending.dequeue_signal(&mask).unwrap().timer_id(), Some(2));
    assert!(pending.dequeue_signal(&mask).is_none());
    drop(prepare_record(&origin, signo, 24).unwrap());
    while crate::process_one_ready_timer_record() {}
    assert!(origin.is_availability_valid.load(Ordering::Relaxed));
}

#[def_test(serial)]
fn prepared_record_bypasses_coalescing_and_drain_returns_unique_ownership() {
    for signo in [Signo::SIGUSR1, Signo::SIGRTMIN] {
        let origin = record_origin();
        let mut pending = crate::PendingSignals::default();
        assert!(pending.put_signal(SignalInfo::new_kernel(signo)).unwrap());
        let rejected = pending
            .put_notification(prepare_record(&origin, signo, 23).unwrap())
            .unwrap();
        assert!(rejected.is_none());
        assert!(prepare_record(&origin, signo, 24).is_none());
        drop(rejected);
        drop(pending);
        assert_eq!(origin.completed.load(Ordering::Relaxed), 0);
        drop(prepare_record(&origin, signo, 24).unwrap());
        while crate::process_one_ready_timer_record() {}
    }
}

#[def_test(serial)]
fn prepared_record_target_rejection_and_final_refs_do_not_complete() {
    let origin = record_origin();
    let weak = Arc::downgrade(&origin);
    let target = crate::TimerSignalTarget::process(Weak::new());
    let returned = returned_notification(
        target.send(prepare_record(&origin, Signo::SIGRTMIN, 23).unwrap()),
        TimerSignalRejection::TargetGone,
    );
    assert!(prepare_record(&origin, Signo::SIGRTMIN, 24).is_none());
    assert_eq!(origin.completed.load(Ordering::Relaxed), 0);
    drop(origin);
    assert!(weak.upgrade().is_none());
    drop(returned);
    while crate::process_one_ready_timer_record() {}
    assert_eq!(weak.strong_count(), 0);
}

#[def_test(serial)]
fn prepared_record_detached_discard_releases_without_acknowledgement() {
    for signo in [Signo::SIGUSR1, Signo::SIGRTMIN] {
        let origin = record_origin();
        let mut pending = crate::PendingSignals::default();
        let mut mask = SignalSet::default();
        mask.add(signo);
        assert!(
            pending
                .put_notification(prepare_record(&origin, signo, 23).unwrap())
                .unwrap()
                .is_none()
        );
        let detached = pending.dequeue_notification(&mask).unwrap();
        drop(pending);
        assert!(prepare_record(&origin, signo, 24).is_none());
        drop(detached);
        assert_eq!(origin.completed.load(Ordering::Relaxed), 0);
        drop(prepare_record(&origin, signo, 24).unwrap());
        while crate::process_one_ready_timer_record() {}
    }
}

#[def_test(serial)]
fn prepared_record_ignored_send_returns_live_lease() {
    let thread = setup();
    let origin = record_origin();
    thread
        .proc
        .set_signal_action(
            Signo::SIGUSR1,
            SignalAction {
                disposition: SignalDisposition::Ignore,
                ..Default::default()
            },
        )
        .unwrap();
    let target = thread.timer_target().unwrap();
    let returned = returned_notification(
        target.send(prepare_record(&origin, Signo::SIGUSR1, 23).unwrap()),
        TimerSignalRejection::Ignored,
    );
    assert!(prepare_record(&origin, Signo::SIGUSR1, 24).is_none());
    assert!(thread.pending().is_empty());
    drop(returned);
    assert_eq!(origin.completed.load(Ordering::Relaxed), 0);
    drop(prepare_record(&origin, Signo::SIGUSR1, 24).unwrap());
    while crate::process_one_ready_timer_record() {}
    drop(MANAGER.lock().take());
}

kspin::static_lock! {
    static MANAGER: SpinNoIrq<Option<Arc<ProcessSignalManager>>> = SpinNoIrq::new(None);
}
kspin::static_lock! {
    static ORIGINS: SpinNoIrq<Option<[Arc<TestOrigin>; 2]>> = SpinNoIrq::new(None);
}

struct TestOrigin {
    rejects: bool,
    sequence: u64,
}

impl TimerSignalOrigin for TestOrigin {
    fn complete(&self, timer_id: i32, signal_seq: u64) -> crate::SignalDequeueAction {
        if timer_id != 17 || signal_seq != self.sequence {
            return crate::SignalDequeueAction::Drop;
        }
        let manager = MANAGER.lock().clone().unwrap();
        let _pending = manager.pending();
        COMPLETIONS.fetch_add(1, Ordering::Relaxed);
        if self.rejects {
            crate::SignalDequeueAction::Drop
        } else {
            crate::SignalDequeueAction::Deliver
        }
    }
}

fn notification(signo: Signo, rejected: bool) -> SignalNotification {
    let origin = Arc::downgrade(&ORIGINS.lock().as_ref().unwrap()[usize::from(rejected)]);
    SignalNotification::timer(
        signo,
        17,
        0,
        0,
        23,
        origin,
    )
}

fn setup() -> Arc<ThreadSignalManager> {
    let manager = Arc::new(ProcessSignalManager::new(SignalActions::default(), 0));
    let thread = ThreadSignalManager::new(71, manager.clone()).unwrap();
    *MANAGER.lock() = Some(manager);
    *ORIGINS.lock() = Some([
        Arc::new(TestOrigin {
            rejects: false,
            sequence: 23,
        }),
        Arc::new(TestOrigin {
            rejects: true,
            sequence: 23,
        }),
    ]);
    COMPLETIONS.store(0, Ordering::Relaxed);
    thread
}

fn returned_notification(
    outcome: TimerSignalSendOutcome,
    expected: TimerSignalRejection,
) -> SignalNotification {
    match outcome {
        TimerSignalSendOutcome::Rejected {
            reason,
            notification,
            ..
        } if reason == expected => Some(notification),
        _ => None,
    }
    .unwrap()
}

#[def_test(serial)]
fn timer_send_coalescing_returns_receipt_for_unlocked_retry() {
    for private in [false, true] {
        let thread = setup();
        let target = if private {
            thread.timer_target().unwrap()
        } else {
            crate::TimerSignalTarget::process(Arc::downgrade(&thread.proc))
        };
        assert!(matches!(
            target.send(notification(Signo::SIGUSR1, false)),
            TimerSignalSendOutcome::Queued { wake_tid: Some(71) }
        ));
        let outcome = target.send(notification(Signo::SIGUSR1, false));
        assert_eq!(outcome.wake_tid(), Some(71));
        let returned = returned_notification(outcome, TimerSignalRejection::Coalesced);
        assert_eq!(COMPLETIONS.load(Ordering::Relaxed), 0);
        assert!(thread.take_signal().is_some());
        assert_eq!(COMPLETIONS.load(Ordering::Relaxed), 1);
        assert!(matches!(
            target.send(returned),
            TimerSignalSendOutcome::Queued { .. }
        ));
        assert!(thread.take_signal().is_some());
        assert_eq!(COMPLETIONS.load(Ordering::Relaxed), 2);
        drop(MANAGER.lock().take());
    }
}

#[def_test(serial)]
fn timer_send_distinguishes_ignored_from_blocked_queue_admission() {
    for private in [false, true] {
        let thread = setup();
        let target = if private {
            thread.timer_target().unwrap()
        } else {
            crate::TimerSignalTarget::process(Arc::downgrade(&thread.proc))
        };
        thread
            .proc
            .set_signal_action(
                Signo::SIGUSR1,
                SignalAction {
                    disposition: SignalDisposition::Ignore,
                    ..Default::default()
                },
            )
            .unwrap();
        let outcome = target.send(notification(Signo::SIGUSR1, false));
        assert_eq!(outcome.wake_tid(), None);
        let returned = returned_notification(outcome, TimerSignalRejection::Ignored);
        assert!(thread.pending().is_empty());
        assert_eq!(COMPLETIONS.load(Ordering::Relaxed), 0);
        let mut mask = SignalSet::default();
        mask.add(Signo::SIGUSR1);
        thread.set_blocked(mask);
        assert!(matches!(
            target.send(returned),
            TimerSignalSendOutcome::Queued { wake_tid: None }
        ));
        assert!(thread.dequeue_signal(&mask).is_some());
        assert_eq!(COMPLETIONS.load(Ordering::Relaxed), 1);
        drop(MANAGER.lock().take());
    }
}

#[def_test(serial)]
fn timer_send_returns_receipt_after_target_or_configuration_disappears() {
    for private in [false, true] {
        for release_handlers in [false, true] {
            let thread = setup();
            let target = if private {
                thread.timer_target().unwrap()
            } else {
                crate::TimerSignalTarget::process(Arc::downgrade(&thread.proc))
            };
            if release_handlers {
                thread.proc.release_handlers();
            } else {
                thread.exit();
            }
            let outcome = target.send(notification(Signo::SIGUSR1, false));
            assert_eq!(outcome.wake_tid(), None);
            let returned = returned_notification(outcome, TimerSignalRejection::TargetGone);
            assert!(thread.pending().is_empty());
            assert_eq!(COMPLETIONS.load(Ordering::Relaxed), 0);
            let (_, receipt) = returned.into_parts();
            let receipt = receipt.unwrap();
            assert_eq!(receipt.timer_id(), 17);
            assert_eq!(receipt.signal_seq(), 23);
            drop(receipt);
            assert_eq!(COMPLETIONS.load(Ordering::Relaxed), 0);
            drop(thread);
            drop(MANAGER.lock().take());
            let returned = returned_notification(
                target.send(notification(Signo::SIGUSR1, false)),
                TimerSignalRejection::TargetGone,
            );
            drop(returned);
            assert_eq!(COMPLETIONS.load(Ordering::Relaxed), 0);
        }
    }
}

#[def_test(serial)]
fn timer_receipt_preserves_generation_above_abi_width() {
    let thread = setup();
    let sequence = (1u64 << 32) + 23;
    let origin = Arc::new(TestOrigin {
        rejects: false,
        sequence,
    });
    let weak = Arc::downgrade(&origin);
    let notification = SignalNotification::timer(
        Signo::SIGUSR1,
        17,
        0,
        0,
        sequence,
        weak,
    );
    let (info, receipt) = notification.into_parts();
    assert_eq!(info.timer_signal_seq(), Some(23));
    let receipt = receipt.unwrap();
    assert_eq!(receipt.signal_seq(), sequence);
    assert_eq!(receipt.complete(), crate::SignalDequeueAction::Deliver);
    assert_eq!(COMPLETIONS.load(Ordering::Relaxed), 1);
    assert!(thread.pending().is_empty());
    drop(MANAGER.lock().take());
}

#[def_test(serial)]
fn expired_timer_origin_is_not_kept_alive_or_replaced_by_the_receiver() {
    let thread = setup();
    let origin = ORIGINS.lock().as_ref().unwrap()[0].clone();
    let weak = Arc::downgrade(&origin);
    assert!(
        thread
            .send_signal(notification(Signo::SIGUSR1, false))
            .unwrap()
    );
    drop(ORIGINS.lock().take());
    assert_eq!(Arc::strong_count(&origin), 1);
    drop(origin);
    assert!(weak.upgrade().is_none());
    assert!(thread.take_signal().is_none());
    assert_eq!(COMPLETIONS.load(Ordering::Relaxed), 0);
    assert!(thread.pending().is_empty());
    drop(MANAGER.lock().take());
}

#[def_test(serial)]
fn ordinary_timer_siginfo_has_no_completion_authority() {
    let thread = setup();
    let mut mask = SignalSet::default();
    mask.add(Signo::SIGUSR1);
    assert!(
        thread
            .send_signal(SignalInfo::new_timer(
                Signo::SIGUSR1,
                17,
                0,
                0,
                23
            ))
            .unwrap()
    );
    assert_eq!(thread.dequeue_signal(&mask).unwrap().timer_id(), Some(17));
    assert_eq!(COMPLETIONS.load(Ordering::Relaxed), 0);
    assert!(
        thread
            .send_signal(notification(Signo::SIGUSR1, false))
            .unwrap()
    );
    assert!(thread.dequeue_signal(&mask).is_some());
    assert_eq!(COMPLETIONS.load(Ordering::Relaxed), 1);
    drop(MANAGER.lock().take());
}

#[def_test(serial)]
fn masked_dequeue_completes_every_timer_signo_outside_siglock() {
    let thread = setup();
    let mut count = 0;
    for raw in 1..=crate::MAX_SIGNALS {
        let Some(signo) = Signo::from_repr(raw as u8) else {
            continue;
        };
        if matches!(signo, Signo::SIGKILL | Signo::SIGSTOP) {
            continue;
        }
        let mut mask = SignalSet::default();
        mask.add(signo);
        thread.set_blocked(mask);
        let _ = thread
            .process()
            .send_signal(notification(signo, false))
            .unwrap();
        assert_eq!(thread.dequeue_signal(&mask).unwrap().signo(), signo);
        count += 1;
        assert_eq!(COMPLETIONS.load(Ordering::Relaxed), count);
        assert!(thread.dequeue_signal(&mask).is_none());
    }
    drop(MANAGER.lock().take());
}

#[def_test(serial)]
fn normal_dequeue_skips_rejected_receipt_and_completes_next_once() {
    let thread = setup();
    assert!(
        thread
            .send_signal(notification(Signo::SIGRTMIN, true))
            .unwrap()
    );
    assert!(
        thread
            .send_signal(notification(Signo::SIGRTMIN, false))
            .unwrap()
    );
    assert_eq!(thread.take_signal().unwrap().sig.signo(), Signo::SIGRTMIN);
    assert_eq!(COMPLETIONS.load(Ordering::Relaxed), 2);
    assert!(thread.take_signal().is_none());
    drop(MANAGER.lock().take());
}

#[def_test(serial)]
fn coalesced_timer_ownership_can_be_retried_after_unlocking() {
    for shared in [false, true] {
        let thread = setup();
        let first = thread.proc.siglock().transact(|txn| {
            if shared {
                txn.put_shared(notification(Signo::SIGUSR1, false)).unwrap()
            } else {
                txn.put_thread(thread.slot, notification(Signo::SIGUSR1, false))
                    .unwrap()
            }
        });
        assert!(first.is_none());
        let rejected = thread
            .proc
            .siglock()
            .transact(|txn| {
                if shared {
                    txn.put_shared(notification(Signo::SIGUSR1, false)).unwrap()
                } else {
                    txn.put_thread(thread.slot, notification(Signo::SIGUSR1, false))
                        .unwrap()
                }
            })
            .unwrap();
        assert_eq!(rejected.signo(), Signo::SIGUSR1);
        assert_eq!(COMPLETIONS.load(Ordering::Relaxed), 0);
        assert!(thread.take_signal().is_some());
        assert_eq!(COMPLETIONS.load(Ordering::Relaxed), 1);
        assert!(thread.take_signal().is_none());
        assert!(thread.send_signal(rejected).unwrap());
        assert!(thread.take_signal().is_some());
        assert_eq!(COMPLETIONS.load(Ordering::Relaxed), 2);
        drop(MANAGER.lock().take());
    }
}

#[def_test(serial)]
fn stale_target_returns_timer_ownership_without_publishing_to_successor() {
    let thread = setup();
    thread.proc.siglock().retire(thread.slot);
    let successor = ThreadSignalManager::new(72, thread.proc.clone()).unwrap();
    let rejected = thread
        .proc
        .siglock()
        .transact(|txn| {
            txn.put_thread(thread.slot, notification(Signo::SIGRTMIN, false))
                .unwrap()
        })
        .unwrap();
    assert!(successor.pending().is_empty());
    assert_eq!(COMPLETIONS.load(Ordering::Relaxed), 0);
    assert!(successor.send_signal(rejected).unwrap());
    assert!(successor.take_signal().is_some());
    assert_eq!(COMPLETIONS.load(Ordering::Relaxed), 1);
    assert!(successor.take_signal().is_none());
    drop(MANAGER.lock().take());
}

#[def_test(serial)]
fn ignored_and_released_destinations_do_not_acknowledge_timer_notifications() {
    let thread = setup();
    thread
        .proc
        .set_signal_action(
            Signo::SIGUSR1,
            SignalAction {
                disposition: SignalDisposition::Ignore,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(
        !thread
            .send_signal(notification(Signo::SIGUSR1, false))
            .unwrap()
    );
    assert!(
        thread
            .proc
            .send_signal(notification(Signo::SIGUSR1, false))
            .unwrap()
            .is_none()
    );
    assert!(thread.pending().is_empty());
    assert!(thread.proc.pending().is_empty());
    assert_eq!(COMPLETIONS.load(Ordering::Relaxed), 0);
    let mut mask = SignalSet::default();
    mask.add(Signo::SIGUSR1);
    thread.set_blocked(mask);
    assert!(
        !thread
            .send_signal(notification(Signo::SIGUSR1, false))
            .unwrap()
    );
    assert!(
        thread
            .proc
            .send_signal(notification(Signo::SIGUSR1, false))
            .unwrap()
            .is_none()
    );
    assert!(thread.dequeue_signal(&mask).is_some());
    assert!(thread.dequeue_signal(&mask).is_some());
    assert_eq!(COMPLETIONS.load(Ordering::Relaxed), 2);
    thread.proc.release_handlers();
    assert!(
        !thread
            .send_signal(notification(Signo::SIGRTMIN, false))
            .unwrap()
    );
    assert!(
        thread
            .proc
            .send_signal(notification(Signo::SIGRTMIN, false))
            .unwrap()
            .is_none()
    );
    assert_eq!(COMPLETIONS.load(Ordering::Relaxed), 2);
    drop(MANAGER.lock().take());
}

#[def_test(serial)]
fn detached_timer_discard_does_not_acknowledge_or_consume_successor() {
    for signo in [Signo::SIGUSR1, Signo::SIGRTMIN] {
        let thread = setup();
        assert!(thread.send_signal(notification(signo, false)).unwrap());
        let detached = thread
            .proc
            .siglock()
            .transact(|txn| txn.take(thread.slot))
            .unwrap();
        assert!(!thread.pending().has(signo));
        assert!(thread.send_signal(notification(signo, false)).unwrap());
        drop(detached);
        assert_eq!(COMPLETIONS.load(Ordering::Relaxed), 0);
        assert!(thread.pending().has(signo));
        assert_eq!(thread.take_signal().unwrap().sig.signo(), signo);
        assert_eq!(COMPLETIONS.load(Ordering::Relaxed), 1);
        assert!(thread.take_signal().is_none());
        drop(MANAGER.lock().take());
    }
}

#[def_test(serial)]
fn coalescing_and_teardown_do_not_invoke_dequeue_completion() {
    let thread = setup();
    assert!(
        thread
            .send_signal(notification(Signo::SIGUSR1, false))
            .unwrap()
    );
    let _ = thread
        .send_signal(notification(Signo::SIGUSR1, false))
        .unwrap();
    assert_eq!(COMPLETIONS.load(Ordering::Relaxed), 0);
    assert!(thread.take_signal().is_some());
    assert_eq!(COMPLETIONS.load(Ordering::Relaxed), 1);
    assert!(
        thread
            .send_signal(notification(Signo::SIGUSR1, false))
            .unwrap()
    );
    drop(thread);
    drop(MANAGER.lock().take());
    assert_eq!(COMPLETIONS.load(Ordering::Relaxed), 1);
}

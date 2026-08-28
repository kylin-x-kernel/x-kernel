// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kcpu_id_map::LogicalCpuId;
use unittest::{assert, assert_eq, def_test};

use crate::{
    CancelPendingResult, CancelWorkResult, ClaimResult, ExecutorOp, FinishResult, QueueWorkError,
    QueueWorkOutcome, Work, WorkQueue, WorkStatus,
};

#[def_test]
fn work_key_is_stable_per_work_and_generation_distinguishes_objects() {
    static FIRST: Work = Work::new();
    static SECOND: Work = Work::new();

    let first = FIRST.key();
    assert_eq!(first, FIRST.key());
    assert_ne!(first, SECOND.key());
    assert_ne!(first.generation(), SECOND.key().generation());
}

#[def_test]
fn queue_work_active_returns_runnable_entry_and_counts_in_flight() {
    static TEST_WQ: WorkQueue<1, 4> = WorkQueue::new("test_wq_active", 1);
    static WORK: Work = Work::new();
    let binding = TEST_WQ.binding(LogicalCpuId::new(0)).unwrap();
    let result = binding.queue_work(&WORK).expect("queue should succeed");
    assert!(matches!(
        result,
        QueueWorkOutcome::Runnable(ExecutorOp::EnqueueRunnable(_))
    ));
    assert_eq!(WORK.status(), WorkStatus::Pending);
}

#[def_test]
fn queue_work_rejects_duplicate_pending() {
    static TEST_WQ: WorkQueue<1, 4> = WorkQueue::new("test_wq_duplicate", 1);
    static WORK: Work = Work::new();
    let binding = TEST_WQ.binding(LogicalCpuId::new(0)).unwrap();
    assert!(binding.queue_work(&WORK).is_ok());
    assert_eq!(
        binding.queue_work(&WORK),
        Err(QueueWorkError::AlreadyQueued)
    );
}

#[def_test]
fn queue_full_leaves_rejected_work_idle() {
    static TEST_WQ: WorkQueue<1, 1> = WorkQueue::new("test_wq_full", 1);
    static FIRST: Work = Work::new();
    static SECOND: Work = Work::new();
    let binding = TEST_WQ.binding(LogicalCpuId::new(0)).unwrap();

    assert!(binding.queue_work(&FIRST).is_ok());
    assert_eq!(
        binding.queue_work(&SECOND),
        Err(QueueWorkError::PendingFull)
    );
    assert_eq!(SECOND.status(), WorkStatus::Idle);
    assert!(matches!(
        binding.cancel_work(&FIRST),
        CancelWorkResult::CanceledPending { .. }
    ));
}

#[def_test]
fn inactive_when_max_active_reached_returns_inactive_entry() {
    static TEST_WQ: WorkQueue<1, 4> = WorkQueue::new("test_wq_inactive", 1);
    static ACTIVE: Work = Work::new();
    static INACTIVE: Work = Work::new();
    let binding = TEST_WQ.binding(LogicalCpuId::new(0)).unwrap();
    assert!(binding.queue_work(&ACTIVE).is_ok());
    let result = binding.queue_work(&INACTIVE).expect("queue should succeed");
    assert!(matches!(
        result,
        QueueWorkOutcome::Inactive(ExecutorOp::EnqueueInactive(_))
    ));
    assert_eq!(INACTIVE.status(), WorkStatus::Pending);
}

#[def_test]
fn cancel_active_work_promotes_one_inactive_slot() {
    static TEST_WQ: WorkQueue<1, 4> = WorkQueue::new("test_wq_promote_after_cancel", 1);
    static ACTIVE: Work = Work::new();
    static INACTIVE: Work = Work::new();
    let binding = TEST_WQ.binding(LogicalCpuId::new(0)).unwrap();

    assert!(binding.queue_work(&ACTIVE).is_ok());
    let inactive_entry = match binding.queue_work(&INACTIVE).unwrap() {
        QueueWorkOutcome::Inactive(ExecutorOp::EnqueueInactive(entry)) => entry,
        _ => panic!("expected inactive entry"),
    };
    let promoted = match binding.cancel_work(&ACTIVE) {
        CancelWorkResult::CanceledPending {
            promote_op: Some(ExecutorOp::PromoteInactive { owner, budget }),
            ..
        } => {
            assert_eq!(owner, binding.owner());
            assert_eq!(budget, 1);
            budget
        }
        _ => panic!("active cancel should create one promotion"),
    };
    assert_eq!(promoted, 1);
    assert!(binding.commit_promoted(inactive_entry, &INACTIVE));

    assert_eq!(ACTIVE.status(), WorkStatus::Idle);
    assert_eq!(INACTIVE.status(), WorkStatus::Pending);
    assert!(!binding.start_flush().complete());
    assert!(matches!(
        binding.cancel_work(&INACTIVE),
        CancelWorkResult::CanceledPending { .. }
    ));
}

#[def_test]
fn claim_moves_pending_to_running_and_running_requeue_runs_after_finish() {
    static TEST_WQ: WorkQueue<1, 4> = WorkQueue::new("test_wq_claim", 1);
    static WORK: Work = Work::new();
    let binding = TEST_WQ.binding(LogicalCpuId::new(0)).unwrap();
    let entry = match binding.queue_work(&WORK).expect("queue should succeed") {
        QueueWorkOutcome::Runnable(ExecutorOp::EnqueueRunnable(entry)) => entry,
        _ => panic!("expected runnable entry"),
    };
    let claimed = match binding.claim(entry, &WORK, 7, 11) {
        ClaimResult::Run(claimed) => claimed,
        ClaimResult::Stale => panic!("claim should succeed"),
    };

    assert_eq!(WORK.status(), WorkStatus::Running);
    assert_eq!(
        binding.queue_work(&WORK),
        Ok(QueueWorkOutcome::QueuedWhileRunning)
    );
    assert_eq!(
        binding.queue_work(&WORK),
        Err(QueueWorkError::AlreadyQueued)
    );

    let requeue_entry = match binding.finish(&WORK, claimed) {
        FinishResult::Finished {
            requeue_op: Some(ExecutorOp::EnqueueInactive(entry)),
            promote_op: Some(ExecutorOp::PromoteInactive { owner, budget }),
            ..
        } => {
            assert_eq!(owner, binding.owner());
            assert_eq!(budget, 1);
            entry
        }
        FinishResult::Finished { .. } => panic!("finish should submit one requeue and promotion"),
        FinishResult::Stale => panic!("finish should succeed"),
    };
    assert_eq!(WORK.status(), WorkStatus::Pending);
    assert!(binding.commit_promoted(requeue_entry, &WORK));
    let claimed = match binding.claim(requeue_entry, &WORK, 7, 12) {
        ClaimResult::Run(claimed) => claimed,
        ClaimResult::Stale => panic!("requeue entry should claim"),
    };
    assert!(matches!(
        binding.finish(&WORK, claimed),
        FinishResult::Finished {
            requeue_op: None,
            ..
        }
    ));
    assert_eq!(WORK.status(), WorkStatus::Idle);
}

#[def_test]
fn executor_entry_from_other_binding_is_stale() {
    static TEST_WQ: WorkQueue<2, 4> = WorkQueue::new("test_wq_binding_id", 1);
    static WORK: Work = Work::new();
    let source = TEST_WQ.binding(LogicalCpuId::new(0)).unwrap();
    let other = TEST_WQ.binding(LogicalCpuId::new(1)).unwrap();
    let entry = match source.queue_work(&WORK).expect("queue should succeed") {
        QueueWorkOutcome::Runnable(ExecutorOp::EnqueueRunnable(entry)) => entry,
        _ => panic!("expected runnable entry"),
    };

    assert_ne!(source.binding_id(), other.binding_id());
    assert!(matches!(
        other.claim(entry, &WORK, 7, 11),
        ClaimResult::Stale
    ));
    assert_eq!(WORK.status(), WorkStatus::Pending);
    assert!(matches!(
        source.cancel_work(&WORK),
        CancelWorkResult::CanceledPending { .. }
    ));
}

#[def_test]
fn nonblocking_cancel_removes_running_requeue_without_canceling_current() {
    static TEST_WQ: WorkQueue<1, 4> = WorkQueue::new("cancel_running_requeue_wq", 1);
    static WORK: Work = Work::new();
    let binding = TEST_WQ.binding(LogicalCpuId::new(0)).unwrap();
    let entry = match binding.queue_work(&WORK).expect("queue should succeed") {
        QueueWorkOutcome::Runnable(ExecutorOp::EnqueueRunnable(entry)) => entry,
        _ => panic!("expected runnable entry"),
    };
    let claimed = match binding.claim(entry, &WORK, 7, 11) {
        ClaimResult::Run(claimed) => claimed,
        ClaimResult::Stale => panic!("claim should succeed"),
    };
    assert_eq!(
        binding.queue_work(&WORK),
        Ok(QueueWorkOutcome::QueuedWhileRunning)
    );
    assert!(matches!(
        binding.cancel_work_nonblocking(&WORK),
        CancelWorkResult::CanceledRunningRequeue { .. }
    ));
    assert_eq!(WORK.status(), WorkStatus::Running);
    match binding.finish(&WORK, claimed) {
        FinishResult::Finished {
            requeue_op,
            cancel_complete,
            ..
        } => {
            assert!(requeue_op.is_none());
            assert!(!cancel_complete);
        }
        FinishResult::Stale => panic!("finish should succeed"),
    }
    assert_eq!(WORK.status(), WorkStatus::Idle);
}

#[def_test]
fn flush_work_snapshot_waits_for_running_requeue_instance() {
    static TEST_WQ: WorkQueue<1, 4> = WorkQueue::new("flush_running_requeue_wq", 1);
    static WORK: Work = Work::new();
    let binding = TEST_WQ.binding(LogicalCpuId::new(0)).unwrap();
    let entry = match binding.queue_work(&WORK).expect("queue should succeed") {
        QueueWorkOutcome::Runnable(ExecutorOp::EnqueueRunnable(entry)) => entry,
        _ => panic!("expected runnable entry"),
    };
    let claimed = match binding.claim(entry, &WORK, 7, 11) {
        ClaimResult::Run(claimed) => claimed,
        ClaimResult::Stale => panic!("claim should succeed"),
    };
    assert_eq!(
        binding.queue_work(&WORK),
        Ok(QueueWorkOutcome::QueuedWhileRunning)
    );
    let snapshot = binding.flush_work(&WORK);
    assert!(!binding.flush_work_complete(snapshot, &WORK));
    let requeue_entry = match binding.finish(&WORK, claimed) {
        FinishResult::Finished {
            requeue_op: Some(ExecutorOp::EnqueueInactive(entry)),
            ..
        } => entry,
        _ => panic!("finish should submit requeue"),
    };
    assert!(!binding.flush_work_complete(snapshot, &WORK));
    assert!(binding.commit_promoted(requeue_entry, &WORK));
    let claimed = match binding.claim(requeue_entry, &WORK, 8, 12) {
        ClaimResult::Run(claimed) => claimed,
        ClaimResult::Stale => panic!("requeue entry should claim"),
    };
    assert!(!binding.flush_work_complete(snapshot, &WORK));
    assert!(matches!(
        binding.finish(&WORK, claimed),
        FinishResult::Finished { .. }
    ));
    assert!(binding.flush_work_complete(snapshot, &WORK));
}

#[def_test]
fn old_executor_entry_is_stale_after_cancel_and_requeue() {
    static TEST_WQ: WorkQueue<1, 4> = WorkQueue::new("test_wq_old_entry", 1);
    static WORK: Work = Work::new();
    let binding = TEST_WQ.binding(LogicalCpuId::new(0)).unwrap();

    let old_entry = match binding.queue_work(&WORK).unwrap() {
        QueueWorkOutcome::Runnable(ExecutorOp::EnqueueRunnable(entry)) => entry,
        _ => panic!("expected runnable"),
    };
    assert!(matches!(
        binding.cancel_work(&WORK),
        CancelWorkResult::CanceledPending { .. }
    ));
    let new_entry = match binding.queue_work(&WORK).unwrap() {
        QueueWorkOutcome::Runnable(ExecutorOp::EnqueueRunnable(entry)) => entry,
        _ => panic!("expected runnable"),
    };

    assert!(matches!(
        binding.claim(old_entry, &WORK, 1, 1),
        ClaimResult::Stale
    ));
    assert_eq!(WORK.status(), WorkStatus::Pending);
    let claimed = match binding.claim(new_entry, &WORK, 1, 2) {
        ClaimResult::Run(claimed) => claimed,
        ClaimResult::Stale => panic!("new entry should claim"),
    };
    assert!(matches!(
        binding.finish(&WORK, claimed),
        FinishResult::Finished { .. }
    ));
}

#[def_test]
fn cancel_pending_removes_entry_and_releases_accounting() {
    static CANCEL_WQ: WorkQueue<1, 4> = WorkQueue::new("cancel_wq", 1);
    static CANCEL_WORK: Work = Work::new();
    let binding = CANCEL_WQ.binding(LogicalCpuId::new(0)).unwrap();
    assert!(binding.queue_work(&CANCEL_WORK).is_ok());

    match binding.cancel_pending(&CANCEL_WORK) {
        CancelPendingResult::Canceled {
            remove_op,
            promote_op,
        } => {
            assert!(matches!(remove_op, ExecutorOp::Remove(_)));
            assert!(promote_op.is_some());
        }
        _ => panic!("cancel should succeed"),
    }
    assert_eq!(CANCEL_WORK.status(), WorkStatus::Idle);
}

#[def_test]
fn flush_snapshot_ignores_later_color() {
    static FLUSH_WQ: WorkQueue<1, 4> = WorkQueue::new("flush_wq", 2);
    static BEFORE: Work = Work::new();
    static AFTER: Work = Work::new();
    let binding = FLUSH_WQ.binding(LogicalCpuId::new(0)).unwrap();

    let before = match binding.queue_work(&BEFORE).unwrap() {
        QueueWorkOutcome::Runnable(ExecutorOp::EnqueueRunnable(entry)) => entry,
        _ => panic!("expected runnable"),
    };
    let snapshot = binding.start_flush();
    assert!(!snapshot.complete());
    assert!(binding.queue_work(&AFTER).is_ok());

    let claimed = match binding.claim(before, &BEFORE, 1, 1) {
        ClaimResult::Run(claimed) => claimed,
        ClaimResult::Stale => panic!("claim should succeed"),
    };
    assert!(matches!(
        binding.finish(&BEFORE, claimed),
        FinishResult::Finished { .. }
    ));
    assert!(binding.flush_complete(snapshot));
}

#[def_test]
fn delayed_pending_does_not_enter_flush_until_activated() {
    static DELAYED_WQ: WorkQueue<1, 4> = WorkQueue::new("delayed_wq", 1);
    static DELAYED: Work = Work::new();
    let binding = DELAYED_WQ.binding(LogicalCpuId::new(0)).unwrap();

    assert!(binding.mark_delayed(&DELAYED).is_ok());
    assert_eq!(DELAYED.status(), WorkStatus::DelayedPending);
    assert!(binding.start_flush().complete());

    let result = binding.activate_delayed(&DELAYED).unwrap();
    assert!(matches!(
        result,
        QueueWorkOutcome::Runnable(ExecutorOp::EnqueueRunnable(_))
    ));
    assert_eq!(DELAYED.status(), WorkStatus::Pending);
}

#[def_test]
fn disabled_work_rejects_queue_and_delayed_activation_until_enabled() {
    static DISABLE_WQ: WorkQueue<1, 4> = WorkQueue::new("disable_wq", 1);
    static WORK: Work = Work::new();
    let binding = DISABLE_WQ.binding(LogicalCpuId::new(0)).unwrap();

    assert_eq!(WORK.disable(), Ok(1));
    assert!(WORK.is_disabled());
    assert_eq!(binding.queue_work(&WORK), Err(QueueWorkError::Disabled));
    assert_eq!(binding.mark_delayed(&WORK), Err(QueueWorkError::Disabled));
    assert_eq!(WORK.enable(), Ok(0));
    assert!(!WORK.is_disabled());

    assert!(binding.mark_delayed(&WORK).is_ok());
    assert_eq!(WORK.disable(), Ok(1));
    assert_eq!(
        binding.activate_delayed(&WORK),
        Err(QueueWorkError::Disabled)
    );
    assert_eq!(WORK.enable(), Ok(0));
    assert!(binding.activate_delayed(&WORK).is_ok());
}

#[def_test]
fn queued_work_reports_already_queued_even_when_disabled() {
    static DISABLE_PENDING_WQ: WorkQueue<1, 4> = WorkQueue::new("disable_pending_wq", 1);
    static WORK: Work = Work::new();
    let binding = DISABLE_PENDING_WQ.binding(LogicalCpuId::new(0)).unwrap();

    assert!(binding.queue_work(&WORK).is_ok());
    assert_eq!(WORK.disable(), Ok(1));
    assert_eq!(
        binding.queue_work(&WORK),
        Err(QueueWorkError::AlreadyQueued)
    );
    assert_eq!(
        binding.mark_delayed(&WORK),
        Err(QueueWorkError::AlreadyQueued)
    );
}

#[def_test]
fn flush_work_waits_for_same_pending_or_running_instance() {
    static FLUSH_WORK_WQ: WorkQueue<1, 4> = WorkQueue::new("flush_work_wq", 1);
    static WORK: Work = Work::new();
    let binding = FLUSH_WORK_WQ.binding(LogicalCpuId::new(0)).unwrap();

    let entry = match binding.queue_work(&WORK).unwrap() {
        QueueWorkOutcome::Runnable(ExecutorOp::EnqueueRunnable(entry)) => entry,
        _ => panic!("expected runnable"),
    };
    let snapshot = binding.flush_work(&WORK);
    assert!(!snapshot.complete());

    let claimed = match binding.claim(entry, &WORK, 2, 3) {
        ClaimResult::Run(claimed) => claimed,
        ClaimResult::Stale => panic!("claim should succeed"),
    };
    assert!(!binding.flush_work_complete(snapshot, &WORK));
    assert!(matches!(
        binding.finish(&WORK, claimed),
        FinishResult::Finished { .. }
    ));
    assert!(binding.flush_work_complete(snapshot, &WORK));
}

#[def_test]
fn cancel_work_marks_running_and_finish_reports_cancel_completion() {
    static CANCEL_RUNNING_WQ: WorkQueue<1, 4> = WorkQueue::new("cancel_running_wq", 1);
    static WORK: Work = Work::new();
    let binding = CANCEL_RUNNING_WQ.binding(LogicalCpuId::new(0)).unwrap();

    let entry = match binding.queue_work(&WORK).unwrap() {
        QueueWorkOutcome::Runnable(ExecutorOp::EnqueueRunnable(entry)) => entry,
        _ => panic!("expected runnable"),
    };
    let claimed = match binding.claim(entry, &WORK, 4, 5) {
        ClaimResult::Run(claimed) => claimed,
        ClaimResult::Stale => panic!("claim should succeed"),
    };
    let snapshot = match binding.cancel_work(&WORK) {
        CancelWorkResult::WaitingRunning(snapshot) => snapshot,
        _ => panic!("running cancel should wait for finish"),
    };
    assert!(!binding.flush_work_complete(snapshot, &WORK));

    match binding.finish(&WORK, claimed) {
        FinishResult::Finished {
            cancel_complete, ..
        } => assert!(cancel_complete),
        FinishResult::Stale => panic!("finish should succeed"),
    }
    assert!(binding.flush_work_complete(snapshot, &WORK));
}

#[def_test]
fn nonblocking_cancel_reports_running_without_canceling_it() {
    static CANCEL_RUNNING_WQ: WorkQueue<1, 4> = WorkQueue::new("nonblocking_cancel_running_wq", 1);
    static WORK: Work = Work::new();
    let binding = CANCEL_RUNNING_WQ.binding(LogicalCpuId::new(0)).unwrap();

    let entry = match binding.queue_work(&WORK).unwrap() {
        QueueWorkOutcome::Runnable(ExecutorOp::EnqueueRunnable(entry)) => entry,
        _ => panic!("expected runnable"),
    };
    let claimed = match binding.claim(entry, &WORK, 7, 11) {
        ClaimResult::Run(claimed) => claimed,
        ClaimResult::Stale => panic!("claim should succeed"),
    };
    assert!(matches!(
        binding.cancel_work_nonblocking(&WORK),
        CancelWorkResult::WaitingRunning(_)
    ));

    match binding.finish(&WORK, claimed) {
        FinishResult::Finished {
            cancel_complete, ..
        } => assert!(!cancel_complete),
        FinishResult::Stale => panic!("finish should succeed"),
    }
}

#[def_test]
fn cancel_work_cancels_delayed_without_executor_op() {
    static CANCEL_DELAYED_WQ: WorkQueue<1, 4> = WorkQueue::new("cancel_delayed_wq", 1);
    static WORK: Work = Work::new();
    let binding = CANCEL_DELAYED_WQ.binding(LogicalCpuId::new(0)).unwrap();

    assert!(binding.mark_delayed(&WORK).is_ok());
    assert!(matches!(
        binding.cancel_work(&WORK),
        CancelWorkResult::CanceledDelayed
    ));
    assert_eq!(WORK.status(), WorkStatus::Idle);
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unit tests for ksignal

#![cfg(unittest)]

use alloc::{
    format,
    sync::{Arc, Weak},
    task::Wake,
};
use core::{
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    task::{Context, Waker},
};

use ktime_types::TimeSpan;
use unittest::{assert, assert_eq, def_test};

use crate::{
    ChildExitInfo, ChildExitSignalInfo, DefaultSignalAction, PendingSignals, SignalAction,
    SignalActionFlags, SignalDequeueAction, SignalDisposition, SignalInfo, SignalSet, SignalStack,
    Signo,
    api::{ProcessSignalManager, SignalActions, ThreadSignalManager},
};

fn new_thread_manager() -> alloc::sync::Arc<ThreadSignalManager> {
    let proc = alloc::sync::Arc::new(ProcessSignalManager::new(SignalActions::default(), 0));
    ThreadSignalManager::new(1, proc).unwrap()
}

#[def_test]
fn fault_signals_carry_code_and_fault_address() {
    let address = 0x2000_usize;
    let cases = [
        (Signo::SIGBUS, linux_raw_sys::general::BUS_ADRERR, address),
        (Signo::SIGSEGV, linux_raw_sys::general::SEGV_MAPERR, address),
        (Signo::SIGSEGV, linux_raw_sys::general::SEGV_ACCERR, address),
    ];
    for (signo, code, address) in cases {
        let info = SignalInfo::new_fault(signo, code as i32, address);
        assert_eq!(info.signo(), signo);
        assert_eq!(info.code(), code as i32);
        assert_eq!(info.fault_address(), Some(address));
    }
}

#[def_test]
fn realtime_queue_exhaustion_preserves_existing_pending_state() {
    for code in [
        linux_raw_sys::general::SI_QUEUE,
        linux_raw_sys::general::SI_TKILL,
        linux_raw_sys::general::SI_TIMER,
        linux_raw_sys::general::SI_KERNEL as i32,
    ] {
        for has_existing in [false, true] {
            let mut pending = PendingSignals::default();
            let mut mask = SignalSet::default();
            mask.add(Signo::SIGRTMIN);
            if has_existing {
                assert!(
                    pending
                        .put_signal(SignalInfo::new_user(Signo::SIGRTMIN, 42, 123))
                        .unwrap()
                );
            }
            let result = pending.put_notification_with_allocator(
                SignalInfo::new_user(Signo::SIGRTMIN, code, 456).into(),
                |_| None,
            );
            assert!(matches!(result, Err(kerrno::KError::WouldBlock)));
            assert_eq!(pending.set.has(Signo::SIGRTMIN), has_existing);
            if has_existing {
                assert_eq!(pending.dequeue_signal(&mask).unwrap().code(), 42);
            }
            assert!(pending.dequeue_signal(&mask).is_none());
        }
    }
}

#[def_test]
fn explicit_kernel_fallback_is_not_inferred_from_siginfo_code() {
    let mut pending = PendingSignals::default();
    let info = SignalInfo::new_kernel(Signo::SIGRTMIN);
    assert!(matches!(
        pending.put_notification_with_allocator(info.clone().into(), |_| None),
        Err(kerrno::KError::WouldBlock)
    ));
    assert!(pending.set.is_empty());
    assert!(
        pending
            .put_notification_with_allocator(
                crate::SignalNotification::kernel_fallback(info),
                |_| None,
            )
            .unwrap()
            .is_none()
    );
    let mut mask = SignalSet::default();
    mask.add(Signo::SIGRTMIN);
    let (info, receipt) = pending.dequeue_notification(&mask).unwrap().into_parts();
    assert_eq!(info.code(), linux_raw_sys::general::SI_USER as i32);
    assert!(receipt.is_none());
    assert!(pending.set.is_empty());
}

#[def_test]
fn standard_owner_failure_preserves_bit_and_coalesces_without_retry() {
    let mut pending = PendingSignals::default();
    let mut mask = SignalSet::default();
    mask.add(Signo::SIGUSR1);
    let allocations = core::cell::Cell::new(0);
    for attempt in 0..2 {
        let rejected = pending
            .put_notification_with_allocator(
                SignalInfo::new_user(Signo::SIGUSR1, linux_raw_sys::general::SI_QUEUE as _, 123)
                    .into(),
                |node| {
                    allocations.set(allocations.get() + 1);
                    if attempt == 0 {
                        None
                    } else {
                        kalloc_types::KArc::new_nowait(node).ok()
                    }
                },
            )
            .unwrap();
        assert_eq!(rejected.is_some(), attempt != 0);
    }
    assert_eq!(allocations.get(), 1);
    assert!(pending.has_deliverable(SignalSet::default()));
    assert!(!pending.has_deliverable(mask));
    let (info, receipt) = pending.dequeue_notification(&mask).unwrap().into_parts();
    assert_eq!(info.signo(), Signo::SIGUSR1);
    assert_eq!(info.code(), linux_raw_sys::general::SI_USER as i32);
    assert_eq!(info.errno(), 0);
    assert!(info.sigval().is_none());
    assert!(receipt.is_none());
    assert!(pending.set.is_empty());
    assert!(pending.dequeue_signal(&mask).is_none());
}

#[def_test]
fn realtime_user_owner_failure_preserves_one_bit_without_coalescing_admission() {
    let mut pending = PendingSignals::default();
    let allocations = core::cell::Cell::new(0);
    let mut mask = SignalSet::default();
    mask.add(Signo::SIGRTMIN);
    for _ in 0..2 {
        assert!(
            pending
                .put_notification_with_allocator(
                    SignalInfo::new_user(
                        Signo::SIGRTMIN,
                        linux_raw_sys::general::SI_USER as _,
                        123
                    )
                    .into(),
                    |_| {
                        allocations.set(allocations.get() + 1);
                        None
                    },
                )
                .unwrap()
                .is_none()
        );
    }
    assert_eq!(allocations.get(), 2);
    assert!(pending.has_deliverable(SignalSet::default()));
    assert!(!pending.has_deliverable(mask));
    let (info, receipt) = pending.dequeue_notification(&mask).unwrap().into_parts();
    assert_eq!(info.signo(), Signo::SIGRTMIN);
    assert_eq!(info.code(), linux_raw_sys::general::SI_USER as i32);
    assert_eq!(info.errno(), 0);
    assert!(receipt.is_none());
    assert!(pending.set.is_empty());
    assert!(pending.dequeue_signal(&mask).is_none());
}

#[def_test]
fn realtime_user_fallback_does_not_add_an_occurrence_beside_detailed_records() {
    for fallback_first in [false, true] {
        let mut pending = PendingSignals::default();
        let mut mask = SignalSet::default();
        mask.add(Signo::SIGRTMIN);
        for attempt in 0..3 {
            let should_fail = if fallback_first {
                attempt == 0
            } else {
                attempt == 2
            };
            assert!(
                pending
                    .put_notification_with_allocator(
                        SignalInfo::new_user(
                            Signo::SIGRTMIN,
                            linux_raw_sys::general::SI_USER as _,
                            123,
                        )
                        .into(),
                        |node| {
                            if should_fail {
                                None
                            } else {
                                Some(kalloc_types::KArc::new_nowait(node).unwrap())
                            }
                        },
                    )
                    .unwrap()
                    .is_none()
            );
        }
        for remaining in [true, false] {
            let notification = pending.dequeue_notification(&mask).unwrap();
            assert!(matches!(
                notification,
                crate::pending::DetachedNotification::Stored(_)
            ));
            let (info, receipt) = notification.into_parts();
            assert_eq!(info.signo(), Signo::SIGRTMIN);
            assert!(receipt.is_none());
            assert_eq!(pending.set.has(Signo::SIGRTMIN), remaining);
        }
        assert!(pending.dequeue_signal(&mask).is_none());
    }
}

#[def_test]
fn ordinary_sigkill_does_not_request_owner_storage() {
    let mut pending = PendingSignals::default();
    let allocations = core::cell::Cell::new(0);
    assert!(
        pending
            .put_notification_with_allocator(SignalInfo::new_kernel(Signo::SIGKILL).into(), |_| {
                allocations.set(allocations.get() + 1);
                None
            },)
            .unwrap()
            .is_none()
    );
    assert_eq!(allocations.get(), 0);
    let mut mask = SignalSet::default();
    mask.add(Signo::SIGKILL);
    let info = pending.dequeue_signal(&mask).unwrap();
    assert_eq!(info.signo(), Signo::SIGKILL);
    assert_eq!(info.code(), linux_raw_sys::general::SI_USER as i32);
    assert!(pending.set.is_empty());
}

#[def_test]
fn sigkill_bit_only_admission_reaches_both_manager_queues() {
    for private in [false, true] {
        let thread = new_thread_manager();
        if private {
            assert!(
                thread
                    .send_signal(SignalInfo::new_kernel(Signo::SIGKILL))
                    .unwrap()
            );
        } else {
            assert_eq!(
                thread
                    .process()
                    .send_signal(SignalInfo::new_kernel(Signo::SIGKILL))
                    .unwrap(),
                Some(1)
            );
        }
        assert!(thread.pending().has(Signo::SIGKILL));
        let mut mask = SignalSet::default();
        mask.add(Signo::SIGKILL);
        assert_eq!(
            thread.dequeue_signal(&mask).unwrap().code(),
            linux_raw_sys::general::SI_USER as i32
        );
        assert!(thread.pending().is_empty());
    }
}

#[def_test]
fn timer_private_target_rejects_exit_and_reused_slot() {
    let thread = new_thread_manager();
    let process = thread.process().clone();
    let target = thread.timer_target().unwrap();
    assert!(target.matches_thread(&thread));
    thread.exit();
    let fatal_before = process.fatal_signal_snapshot();
    assert_eq!(
        target
            .send(SignalInfo::new_kernel(Signo::SIGKILL).into())
            .wake_tid(),
        None
    );
    assert_eq!(process.fatal_signal_snapshot(), fatal_before);
    assert!(thread.pending().is_empty());
    assert!(!target.matches_thread(&thread));
    drop(thread);
    let successor = ThreadSignalManager::new(1, process).unwrap();
    assert!(!target.matches_thread(&successor));
    assert_eq!(
        target
            .send(SignalInfo::new_kernel(Signo::SIGUSR1).into())
            .wake_tid(),
        None
    );
    assert!(successor.pending().is_empty());
    let fresh = successor.timer_target().unwrap();
    assert_eq!(
        fresh
            .send(SignalInfo::new_kernel(Signo::SIGUSR1).into())
            .wake_tid(),
        Some(1)
    );
    assert!(successor.pending().has(Signo::SIGUSR1));
}

#[def_test]
fn timer_target_follows_slot_rebind_not_original_number() {
    let thread = new_thread_manager();
    let target = thread.timer_target().unwrap();
    thread.rebind_after_exec(2);
    let successor = ThreadSignalManager::new(1, thread.process().clone()).unwrap();
    assert_eq!(
        target
            .send(SignalInfo::new_kernel(Signo::SIGUSR1).into())
            .wake_tid(),
        Some(2)
    );
    assert!(thread.pending().has(Signo::SIGUSR1));
    assert!(successor.pending().is_empty());
    assert!(target.matches_thread(&thread));
    assert!(!target.matches_thread(&successor));
}

#[def_test]
fn timer_shared_target_preserves_blocking_and_rejects_empty_or_expired_manager() {
    let thread = new_thread_manager();
    let process = thread.process().clone();
    let target = crate::TimerSignalTarget::process(Arc::downgrade(&process));
    let unrelated = new_thread_manager();
    assert!(!target.matches_thread(&unrelated));
    let mut mask = SignalSet::default();
    mask.add(Signo::SIGUSR1);
    thread.set_blocked(mask);
    assert_eq!(
        target
            .send(SignalInfo::new_kernel(Signo::SIGUSR1).into())
            .wake_tid(),
        None
    );
    assert!(process.pending().has(Signo::SIGUSR1));
    assert!(thread.dequeue_signal(&mask).is_some());
    thread.exit();
    assert_eq!(
        target
            .send(SignalInfo::new_kernel(Signo::SIGUSR1).into())
            .wake_tid(),
        None
    );
    assert!(process.pending().is_empty());
    drop(thread);
    let weak = Arc::downgrade(&process);
    drop(process);
    assert!(weak.upgrade().is_none());
    assert_eq!(
        target
            .send(SignalInfo::new_kernel(Signo::SIGUSR1).into())
            .wake_tid(),
        None
    );
    assert!(unrelated.pending().is_empty());
}

#[def_test]
fn slot_generation_high_bits_prevent_stale_index_reuse() {
    let lock = crate::SigLock::new();
    let (stale, _) = lock.register(31, kalloc_types::AllocFlags::NOWAIT).unwrap();
    lock.retire(stale);
    lock.set_next_generation_for_test(1u64 << 32);
    let (successor, hint) = lock.register(32, kalloc_types::AllocFlags::NOWAIT).unwrap();
    assert!(stale != successor);
    let rejected = lock.transact(|txn| {
        txn.put_thread(stale, SignalInfo::new_kernel(Signo::SIGUSR1))
            .unwrap()
    });
    assert!(rejected.is_some());
    assert!(!hint.load(Ordering::Acquire));
    lock.retire(stale);
    let rejected = lock.transact(|txn| {
        txn.put_thread(successor, SignalInfo::new_kernel(Signo::SIGUSR1))
            .unwrap()
    });
    assert!(rejected.is_none());
    assert!(hint.load(Ordering::Acquire));
    lock.retire(successor);
}

#[def_test]
fn exhausted_registration_preserves_signal_state_and_allows_retirement() {
    let lock = crate::SigLock::new();
    lock.set_next_generation_for_test(u64::MAX);
    let (last, hint) = lock.register(41, kalloc_types::AllocFlags::NOWAIT).unwrap();
    let rejected = lock.transact(|txn| {
        txn.put_thread(last, SignalInfo::new_kernel(Signo::SIGUSR1))
            .unwrap()
    });
    assert!(rejected.is_none());
    assert!(matches!(
        lock.register(42, kalloc_types::AllocFlags::NOWAIT),
        Err(kerrno::KError::InvalidInput)
    ));
    assert!(hint.load(Ordering::Acquire));
    assert!(lock.inspect(|view| view.pending(last)).has(Signo::SIGUSR1));
    assert_eq!(Arc::strong_count(&hint), 2);
    lock.retire(last);
    assert_eq!(Arc::strong_count(&hint), 1);
    assert_eq!(
        lock.inspect(|view| view.find_target(Signo::SIGUSR1)),
        (None, false)
    );
    assert!(matches!(
        lock.register(43, kalloc_types::AllocFlags::NOWAIT),
        Err(kerrno::KError::InvalidInput)
    ));
    assert_eq!(
        lock.inspect(|view| view.find_target(Signo::SIGUSR1)),
        (None, false)
    );
}

#[def_test]
fn allocation_policy_failure_does_not_consume_last_slot_generation() {
    let lock = crate::SigLock::new();
    lock.set_next_generation_for_test(u64::MAX);
    let invalid = kalloc_types::AllocFlags::from_bits_retain(1 << 31);
    assert!(matches!(
        lock.register(51, invalid),
        Err(kerrno::KError::InvalidInput)
    ));
    let (last, _) = lock.register(52, kalloc_types::AllocFlags::NOWAIT).unwrap();
    lock.retire(last);
    assert!(matches!(
        lock.register(53, kalloc_types::AllocFlags::NOWAIT),
        Err(kerrno::KError::InvalidInput)
    ));
}

#[def_test]
fn test_slot_policy_failure_does_not_publish_registration() {
    let lock = crate::SigLock::new();
    let invalid = kalloc_types::AllocFlags::from_bits_retain(1 << 31);
    assert!(matches!(
        lock.register(41, invalid),
        Err(kerrno::KError::InvalidInput)
    ));
    assert_eq!(
        lock.inspect(|view| view.find_target(Signo::SIGUSR1)),
        (None, false)
    );
    let (slot, hint) = lock.register(42, kalloc_types::AllocFlags::NOWAIT).unwrap();
    assert!(!hint.load(Ordering::Acquire));
    assert_eq!(
        lock.inspect(|view| view.find_target(Signo::SIGUSR1)).0,
        Some(42)
    );
    lock.retire(slot);
    assert_eq!(
        lock.inspect(|view| view.find_target(Signo::SIGUSR1)),
        (None, false)
    );
}

#[def_test]
fn test_detached_slot_keeps_ownership_without_retaining_registration() {
    let lock = crate::SigLock::new();
    let (slot, hint) = lock.register(61, kalloc_types::AllocFlags::NOWAIT).unwrap();
    let _rejected = lock.transact(|txn| {
        txn.put_thread(slot, SignalInfo::new_kernel(Signo::SIGUSR1))
            .unwrap()
    });
    let _rejected = lock.transact(|txn| {
        txn.put_shared(SignalInfo::new_kernel(Signo::SIGRTMIN))
            .unwrap()
    });
    assert_eq!(Arc::strong_count(&hint), 2);

    let (retired, retained_references, target, pending) = lock.transact(|txn| {
        let retired = txn.detach_slot(slot);
        (
            retired,
            Arc::strong_count(&hint),
            txn.view().find_target(Signo::SIGUSR1),
            txn.view().pending(slot),
        )
    });
    assert!(retired.is_some());
    assert_eq!(retained_references, 2);
    assert_eq!(target, (None, false));
    assert!(pending.is_empty());
    let (successor, successor_hint) = lock.register(62, kalloc_types::AllocFlags::NOWAIT).unwrap();
    assert!(successor_hint.load(Ordering::Acquire));
    lock.retire(slot);
    assert_eq!(
        lock.inspect(|view| view.find_target(Signo::SIGUSR1)),
        (Some(62), false)
    );
    assert!(lock.inspect(|view| view.pending(successor)).is_empty());
    assert!(
        lock.inspect(|view| view.shared_pending())
            .has(Signo::SIGRTMIN)
    );

    drop(retired);
    assert_eq!(Arc::strong_count(&hint), 1);
    lock.retire(successor);
    assert_eq!(Arc::strong_count(&successor_hint), 1);
}

#[def_test]
fn test_standard_dequeue_transfers_storage_before_queue_reuse() {
    let mut pending = PendingSignals::default();
    let mut mask = SignalSet::default();
    mask.add(Signo::SIGUSR1);
    assert!(
        pending
            .put_signal(SignalInfo::new_kernel(Signo::SIGUSR1))
            .unwrap()
    );
    let detached = pending.dequeue_notification(&mask).unwrap();
    assert!(!pending.set.has(Signo::SIGUSR1));
    assert!(
        pending
            .put_signal(SignalInfo::new_kernel(Signo::SIGUSR1))
            .unwrap()
    );
    let (info, receipt) = detached.into_parts();
    assert_eq!(info.signo(), Signo::SIGUSR1);
    assert!(receipt.is_none());
    assert!(pending.set.has(Signo::SIGUSR1));
    assert_eq!(
        pending.dequeue_signal(&mask).unwrap().signo(),
        Signo::SIGUSR1
    );
    assert!(pending.dequeue_signal(&mask).is_none());
}

#[def_test]
fn test_manager_policy_failure_releases_process_reference() {
    let process = Arc::new(ProcessSignalManager::new(SignalActions::default(), 0));
    let references = Arc::strong_count(&process);
    let invalid = kalloc_types::AllocFlags::from_bits_retain(1 << 31);
    assert!(matches!(
        ThreadSignalManager::new_with_flags(51, process.clone(), invalid),
        Err(kerrno::KError::InvalidInput)
    ));
    assert_eq!(Arc::strong_count(&process), references);
    let thread = ThreadSignalManager::new_nowait(51, process.clone()).unwrap();
    drop(thread);
    assert_eq!(Arc::strong_count(&process), references);
}

struct PendingWake {
    reader: Weak<ThreadSignalManager>,
    signo: Signo,
    calls: AtomicUsize,
    saw_pending: AtomicBool,
}

impl Wake for PendingWake {
    fn wake(self: Arc<Self>) {
        if let Some(reader) = self.reader.upgrade() {
            // Reenter both pending and ownership paths before recording the wake.
            self.saw_pending
                .store(reader.pending().has(self.signo), Ordering::Relaxed);
            let _ = reader.process().signal_action(self.signo);
        }
        self.calls.fetch_add(1, Ordering::Relaxed);
    }
}

fn observe_pending(
    source: &kpoll::WeakPollSet,
    reader: &Arc<ThreadSignalManager>,
    signo: Signo,
) -> (Arc<PendingWake>, kpoll::PollRegistrations) {
    let observed = Arc::new(PendingWake {
        reader: Arc::downgrade(reader),
        signo,
        calls: AtomicUsize::new(0),
        saw_pending: AtomicBool::new(false),
    });
    let waker = Waker::from(observed.clone());
    let context = Context::from_waker(&waker);
    let mut registrations = kpoll::PollRegistrations::new();
    registrations
        .context(&context)
        .register_weak(source)
        .unwrap();
    (observed, registrations)
}

#[def_test]
fn shared_realtime_owner_failure_does_not_publish_fatal_epoch_or_wake() {
    let receiver = new_thread_manager();
    let process = receiver.process();
    let source = receiver.pending_source().unwrap();
    let (observed, _registration) = observe_pending(&source, &receiver, Signo::SIGRTMIN);
    let before = process.fatal_signal_snapshot();
    let failed = process.send_signal_with_allocator(
        SignalInfo::new_user(Signo::SIGRTMIN, linux_raw_sys::general::SI_QUEUE as _, 123).into(),
        false,
        |_| None,
    );
    assert_eq!(failed.admission_result(), Err(kerrno::KError::WouldBlock));
    assert_eq!(failed.wake_tid(), None);
    assert_eq!(process.fatal_signal_snapshot(), before);
    assert!(receiver.pending().is_empty());
    assert_eq!(observed.calls.load(Ordering::Relaxed), 0);
    assert_eq!(
        process
            .send_signal(SignalInfo::new_user(
                Signo::SIGRTMIN,
                linux_raw_sys::general::SI_QUEUE as _,
                123,
            ))
            .unwrap(),
        Some(1)
    );
    assert!(process.fatal_signal_snapshot() != before);
    assert_eq!(observed.calls.load(Ordering::Relaxed), 1);
    assert!(observed.saw_pending.load(Ordering::Relaxed));
}

#[def_test]
fn realtime_child_exit_owner_failure_does_not_publish_signal() {
    for status in [7 << 8, 9, 11 | 0x80] {
        let receiver = new_thread_manager();
        let process = receiver.process();
        let source = receiver.pending_source().unwrap();
        let (observed, _registration) = observe_pending(&source, &receiver, Signo::SIGRTMIN);
        let before = process.fatal_signal_snapshot();
        let child = ChildExitInfo::from_wait_status(
            42,
            1000,
            status,
            TimeSpan::from_millis(20),
            TimeSpan::from_millis(30),
        );
        let info = ChildExitSignalInfo::new(Signo::SIGRTMIN, child).into_signal_info();
        let failed = process.send_signal_with_allocator(info.into(), false, |_| None);
        assert_eq!(failed.admission_result(), Err(kerrno::KError::WouldBlock));
        assert_eq!(failed.wake_tid(), None);
        assert_eq!(process.fatal_signal_snapshot(), before);
        assert!(receiver.pending().is_empty());
        assert_eq!(observed.calls.load(Ordering::Relaxed), 0);
    }
}

#[def_test]
fn ignored_realtime_admission_does_not_attempt_owner_allocation() {
    let receiver = new_thread_manager();
    receiver
        .process()
        .set_signal_action(
            Signo::SIGRTMIN,
            SignalAction {
                disposition: SignalDisposition::Ignore,
                ..Default::default()
            },
        )
        .unwrap();
    let allocations = core::cell::Cell::new(0);
    let outcome = receiver.process().send_signal_with_allocator(
        SignalInfo::new_user(Signo::SIGRTMIN, linux_raw_sys::general::SI_QUEUE as _, 123).into(),
        false,
        |_| {
            allocations.set(allocations.get() + 1);
            None
        },
    );
    assert_eq!(outcome.admission_result(), Ok(()));
    assert_eq!(outcome.wake_tid(), None);
    assert_eq!(allocations.get(), 0);
    assert!(receiver.pending().is_empty());
}

#[def_test]
fn shared_disposition_changes_apply_to_process_and_private_admission() {
    for is_private in [false, true] {
        let receiver = new_thread_manager();
        let peer = receiver.process().fork_shared().unwrap();
        let signo = Signo::SIGUSR1;
        peer.set_signal_action(
            signo,
            SignalAction {
                disposition: SignalDisposition::Ignore,
                ..Default::default()
            },
        )
        .unwrap();
        let source = receiver.pending_source().unwrap();
        let (observed, _registration) = observe_pending(&source, &receiver, signo);
        let send = || {
            if is_private {
                let _ = receiver.send_signal(SignalInfo::new_kernel(signo)).unwrap();
            } else {
                let _ = receiver
                    .process()
                    .send_signal(SignalInfo::new_kernel(signo))
                    .unwrap();
            }
        };
        send();
        assert!(receiver.pending().is_empty());
        assert_eq!(observed.calls.load(Ordering::Relaxed), 0);

        let old = peer
            .replace_signal_action(signo, Some(SignalAction::default()))
            .unwrap();
        assert!(matches!(old.disposition, SignalDisposition::Ignore));
        send();
        assert!(receiver.pending().has(signo));
        assert_eq!(observed.calls.load(Ordering::Relaxed), 1);
        assert!(observed.saw_pending.load(Ordering::Relaxed));
    }
}

#[def_test]
fn test_shared_arrival_notifications_do_not_share_pending() {
    let parent = new_thread_manager();
    let shared =
        ThreadSignalManager::new(2, Arc::new(parent.process().fork_shared().unwrap())).unwrap();
    let private =
        ThreadSignalManager::new(3, Arc::new(parent.process().fork_private().unwrap())).unwrap();
    let source = parent.pending_source().unwrap();
    let (observed, _registration) = observe_pending(&source, &shared, Signo::SIGUSR1);
    let (isolated, _private_registration) =
        observe_pending(&private.pending_source().unwrap(), &private, Signo::SIGUSR1);
    let _ = shared
        .process()
        .send_signal(SignalInfo::new_kernel(Signo::SIGUSR1))
        .unwrap();
    assert_eq!(observed.calls.load(Ordering::Relaxed), 1);
    assert!(observed.saw_pending.load(Ordering::Relaxed));
    assert!(parent.pending().is_empty());
    assert_eq!(isolated.calls.load(Ordering::Relaxed), 0);

    let (observed, _registration) = observe_pending(&source, &shared, Signo::SIGUSR2);
    let _ = shared
        .send_signal(SignalInfo::new_kernel(Signo::SIGUSR2))
        .unwrap();
    assert_eq!(observed.calls.load(Ordering::Relaxed), 1);
    assert!(observed.saw_pending.load(Ordering::Relaxed));
    assert!(!shared.process().pending().has(Signo::SIGUSR2));
    assert!(parent.pending().is_empty());

    let (observed, _registration) = observe_pending(&source, &parent, Signo::SIGUSR1);
    shared.notify_pending().unwrap();
    assert_eq!(observed.calls.load(Ordering::Relaxed), 1);
    assert!(!observed.saw_pending.load(Ordering::Relaxed));

    let (observed, _registration) = observe_pending(&source, &shared, Signo::SIGWINCH);
    let _ = shared
        .send_signal(SignalInfo::new_kernel(Signo::SIGWINCH))
        .unwrap();
    assert_eq!(observed.calls.load(Ordering::Relaxed), 0);
    let mut blocked = SignalSet::default();
    blocked.add(Signo::SIGWINCH);
    shared.set_blocked(blocked);
    assert!(
        !shared
            .send_signal(SignalInfo::new_kernel(Signo::SIGWINCH))
            .unwrap()
    );
    assert_eq!(observed.calls.load(Ordering::Relaxed), 1);
    assert!(observed.saw_pending.load(Ordering::Relaxed));
}

#[def_test]
fn test_exec_detaches_source_and_release_expires_it_with_manager_alive() {
    let parent = new_thread_manager();
    let child =
        ThreadSignalManager::new(2, Arc::new(parent.process().fork_shared().unwrap())).unwrap();
    let old = child.pending_source().unwrap();
    child.process().reset_actions_for_exec().unwrap();
    let (old_observed, _old_registration) = observe_pending(&old, &child, Signo::SIGUSR1);
    let current = child.pending_source().unwrap();
    let (new_observed, _new_registration) = observe_pending(&current, &child, Signo::SIGUSR1);
    let _ = child
        .send_signal(SignalInfo::new_kernel(Signo::SIGUSR1))
        .unwrap();
    assert_eq!(old_observed.calls.load(Ordering::Relaxed), 0);
    assert_eq!(new_observed.calls.load(Ordering::Relaxed), 1);
    assert!(new_observed.saw_pending.load(Ordering::Relaxed));

    parent.process().release_handlers();
    assert_eq!(old_observed.calls.load(Ordering::Relaxed), 1);
    let (dead_observed, dead_registration) = observe_pending(&old, &child, Signo::SIGUSR1);
    assert!(dead_registration.is_empty());
    child.notify_pending().unwrap();
    assert_eq!(dead_observed.calls.load(Ordering::Relaxed), 0);

    let (released, _registration) = observe_pending(&current, &child, Signo::SIGUSR1);
    child.process().release_handlers();
    assert_eq!(released.calls.load(Ordering::Relaxed), 1);
    assert!(matches!(
        child.pending_source(),
        Err(kerrno::KError::NoSuchProcess)
    ));
    assert_eq!(child.notify_pending(), Err(kerrno::KError::NoSuchProcess));
    let (_, registration) = observe_pending(&current, &child, Signo::SIGUSR1);
    assert!(registration.is_empty());
    let mut registrations = kpoll::PollRegistrations::new();
    let context = Context::from_waker(Waker::noop());
    assert_eq!(
        child.register_pending(&mut registrations.context(&context)),
        Err(kpoll::PollRegisterError::InvalidState)
    );
}

#[def_test]
fn test_sole_owner_exec_preserves_source_and_pending_before_registration() {
    let thread = new_thread_manager();
    let source = thread.pending_source().unwrap();
    let _ = thread
        .send_signal(SignalInfo::new_kernel(Signo::SIGUSR1))
        .unwrap();
    thread.process().reset_actions_for_exec().unwrap();
    let (observed, _registration) = observe_pending(&source, &thread, Signo::SIGUSR1);
    assert_eq!(observed.calls.load(Ordering::Relaxed), 0);
    assert!(thread.pending().has(Signo::SIGUSR1));
    // A repeated standard signal may still request a recheck.
    let _ = thread
        .send_signal(SignalInfo::new_kernel(Signo::SIGUSR1))
        .unwrap();
    assert_eq!(observed.calls.load(Ordering::Relaxed), 1);
    assert!(observed.saw_pending.load(Ordering::Relaxed));
}

#[def_test]
fn test_child_exit_commit_uses_source_current_at_publication() {
    let parent = new_thread_manager();
    let child =
        ThreadSignalManager::new(2, Arc::new(parent.process().fork_shared().unwrap())).unwrap();
    let (old_observed, _old_registration) =
        observe_pending(&child.pending_source().unwrap(), &child, Signo::SIGCHLD);
    let prepared = child
        .process()
        .prepare_child_exit_signal(new_sigchld_child_exit_signal())
        .unwrap();
    assert!(child.pending().is_empty());
    assert_eq!(old_observed.calls.load(Ordering::Relaxed), 0);
    child.process().reset_actions_for_exec().unwrap();
    let (current_observed, _registration) =
        observe_pending(&child.pending_source().unwrap(), &child, Signo::SIGCHLD);
    let _ = child.process().commit_child_exit_signal(prepared);
    assert_eq!(old_observed.calls.load(Ordering::Relaxed), 0);
    assert_eq!(current_observed.calls.load(Ordering::Relaxed), 1);
    assert!(current_observed.saw_pending.load(Ordering::Relaxed));
}

static DEQUEUED_SIGNAL_COUNT: AtomicUsize = AtomicUsize::new(0);
static LAST_DEQUEUED_SIGNAL: AtomicUsize = AtomicUsize::new(0);

#[def_test]
fn test_process_fork_shares_or_copies_complete_actions_not_pending() {
    extern "C" fn handler(_: i32) {}
    extern "C" fn restorer() {}

    let parent = ProcessSignalManager::new(SignalActions::default(), 0);
    let mut mask = SignalSet::default();
    mask.add(Signo::SIGUSR2);
    parent
        .set_signal_action(
            Signo::SIGUSR1,
            SignalAction {
                disposition: SignalDisposition::Handler(handler),
                flags: SignalActionFlags::RESTART | SignalActionFlags::RESTORER,
                mask,
                restorer: Some(restorer),
            },
        )
        .unwrap();
    let _ = parent
        .send_signal(SignalInfo::new_kernel(Signo::SIGTERM))
        .unwrap();
    let shared = parent.fork_shared().unwrap();
    let private = parent.fork_private().unwrap();
    for child in [&shared, &private] {
        let action = child.signal_action(Signo::SIGUSR1).unwrap();
        assert!(matches!(action.disposition, SignalDisposition::Handler(f)
            if f as usize == handler as *const () as usize));
        assert_eq!(
            action.flags.bits(),
            (SignalActionFlags::RESTART | SignalActionFlags::RESTORER).bits()
        );
        assert!(action.mask.has(Signo::SIGUSR2));
        assert_eq!(
            action.restorer.map(|f| f as usize),
            Some(restorer as *const () as usize)
        );
        assert!(child.pending().is_empty());
    }
    shared
        .set_signal_action(Signo::SIGUSR1, SignalAction::default())
        .unwrap();
    let action = parent.signal_action(Signo::SIGUSR1).unwrap();
    assert!(matches!(action.disposition, SignalDisposition::Default));
    assert!(action.flags.is_empty());
    assert!(action.mask.is_empty());
    assert!(action.restorer.is_none());
    assert!(matches!(
        private.signal_action(Signo::SIGUSR1).unwrap().disposition,
        SignalDisposition::Handler(_)
    ));
    private
        .set_signal_action(
            Signo::SIGUSR1,
            SignalAction {
                disposition: SignalDisposition::Ignore,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(matches!(
        parent.signal_action(Signo::SIGUSR1).unwrap().disposition,
        SignalDisposition::Default
    ));
    assert!(parent.pending().has(Signo::SIGTERM));
}

#[def_test]
fn test_fork_cleared_resets_handlers_but_keeps_ignored() {
    extern "C" fn handler(_: i32) {}
    extern "C" fn restorer() {}

    let parent = ProcessSignalManager::new(SignalActions::default(), 0);
    let mut mask = SignalSet::default();
    mask.add(Signo::SIGUSR2);
    parent
        .set_signal_action(
            Signo::SIGUSR1,
            SignalAction {
                disposition: SignalDisposition::Handler(handler),
                flags: SignalActionFlags::RESTART | SignalActionFlags::RESTORER,
                mask,
                restorer: Some(restorer),
            },
        )
        .unwrap();
    parent
        .set_signal_action(
            Signo::SIGUSR2,
            SignalAction {
                disposition: SignalDisposition::Ignore,
                ..Default::default()
            },
        )
        .unwrap();

    let cleared = parent.fork_cleared().unwrap();
    let action = cleared.signal_action(Signo::SIGUSR1).unwrap();
    assert!(matches!(action.disposition, SignalDisposition::Default));
    assert!(action.flags.is_empty());
    assert!(action.mask.is_empty());
    assert!(action.restorer.is_none());
    assert!(matches!(
        cleared.signal_action(Signo::SIGUSR2).unwrap().disposition,
        SignalDisposition::Ignore
    ));
    assert!(cleared.pending().is_empty());

    // The parent's action table is untouched by the cleared fork.
    assert!(matches!(
        parent.signal_action(Signo::SIGUSR1).unwrap().disposition,
        SignalDisposition::Handler(_)
    ));
}

#[def_test]
fn test_exec_reset_keeps_ignored_disposition_and_pending() {
    let process = ProcessSignalManager::new(SignalActions::default(), 0);
    process
        .set_signal_action(
            Signo::SIGUSR1,
            SignalAction {
                disposition: SignalDisposition::Ignore,
                ..Default::default()
            },
        )
        .unwrap();
    process
        .set_signal_action(
            Signo::SIGUSR2,
            SignalAction {
                flags: SignalActionFlags::RESTART,
                ..Default::default()
            },
        )
        .unwrap();
    let _ = process
        .send_signal(SignalInfo::new_kernel(Signo::SIGTERM))
        .unwrap();
    process.reset_actions_for_exec().unwrap();
    assert!(matches!(
        process.signal_action(Signo::SIGUSR1).unwrap().disposition,
        SignalDisposition::Ignore
    ));
    assert!(
        process
            .signal_action(Signo::SIGUSR2)
            .unwrap()
            .flags
            .is_empty()
    );
    assert!(process.pending().has(Signo::SIGTERM));
}

#[def_test]
fn test_exec_resets_ignored_metadata_without_changing_shared_peer() {
    extern "C" fn restorer() {}
    let parent = alloc::sync::Arc::new(ProcessSignalManager::new(SignalActions::default(), 0));
    let mut mask = SignalSet::default();
    mask.add(Signo::SIGUSR2);
    parent
        .set_signal_action(
            Signo::SIGUSR1,
            SignalAction {
                disposition: SignalDisposition::Ignore,
                flags: SignalActionFlags::RESTART | SignalActionFlags::RESTORER,
                mask,
                restorer: Some(restorer),
            },
        )
        .unwrap();
    let child = parent.fork_shared().unwrap();
    let _ = child
        .send_signal(SignalInfo::new_kernel(Signo::SIGTERM))
        .unwrap();
    child.reset_actions_for_exec().unwrap();
    let reset = child.signal_action(Signo::SIGUSR1).unwrap();
    assert!(matches!(reset.disposition, SignalDisposition::Ignore));
    assert!(reset.flags.is_empty());
    assert!(reset.mask.is_empty());
    assert!(reset.restorer.is_none());
    let retained = parent.signal_action(Signo::SIGUSR1).unwrap();
    assert!(retained.flags.contains(SignalActionFlags::RESTART));
    assert!(retained.mask.has(Signo::SIGUSR2));
    assert!(retained.restorer.is_some());
    assert!(child.pending().has(Signo::SIGTERM));
    assert!(parent.pending().is_empty());

    // A later clone must inherit the detached configuration, not the peer's.
    let grandchild = child.fork_shared().unwrap();
    grandchild
        .set_signal_action(Signo::SIGUSR1, SignalAction::default())
        .unwrap();
    assert!(matches!(
        child.signal_action(Signo::SIGUSR1).unwrap().disposition,
        SignalDisposition::Default
    ));
    assert!(matches!(
        parent.signal_action(Signo::SIGUSR1).unwrap().disposition,
        SignalDisposition::Ignore
    ));
    // Retaining the manager for a query is not an extra configuration owner.
    let query = parent.clone();
    parent.reset_actions_for_exec().unwrap();
    assert!(
        query
            .signal_action(Signo::SIGUSR1)
            .unwrap()
            .flags
            .is_empty()
    );
}

#[def_test]
fn test_exec_resets_caught_handler_only_in_executing_process() {
    extern "C" fn handler(_: i32) {}
    let parent = ProcessSignalManager::new(SignalActions::default(), 0);
    parent
        .set_signal_action(
            Signo::SIGUSR1,
            SignalAction {
                disposition: SignalDisposition::Handler(handler),
                ..Default::default()
            },
        )
        .unwrap();
    let child = parent.fork_shared().unwrap();
    child.reset_actions_for_exec().unwrap();
    assert!(matches!(
        child.signal_action(Signo::SIGUSR1).unwrap().disposition,
        SignalDisposition::Default
    ));
    assert!(matches!(
        parent.signal_action(Signo::SIGUSR1).unwrap().disposition,
        SignalDisposition::Handler(_)
    ));
}

fn record_dequeued_signal(sig: &SignalInfo) -> SignalDequeueAction {
    LAST_DEQUEUED_SIGNAL.store(sig.signo() as usize, Ordering::Relaxed);
    DEQUEUED_SIGNAL_COUNT.fetch_add(1, Ordering::Relaxed);
    SignalDequeueAction::Deliver
}

#[def_test]
fn test_child_exit_info_decodes_wait_status() {
    let exited = ChildExitInfo::from_wait_status(
        42,
        1000,
        7 << 8,
        TimeSpan::from_nanos(11),
        TimeSpan::from_nanos(13),
    );
    assert_eq!(exited.code(), linux_raw_sys::general::CLD_EXITED as i32);
    assert_eq!(exited.status(), 7);

    let killed = ChildExitInfo::from_wait_status(
        42,
        1000,
        Signo::SIGTERM as i32,
        TimeSpan::from_nanos(11),
        TimeSpan::from_nanos(13),
    );
    assert_eq!(killed.code(), linux_raw_sys::general::CLD_KILLED as i32);
    assert_eq!(killed.status(), Signo::SIGTERM as i32);

    let dumped = ChildExitInfo::from_wait_status(
        42,
        1000,
        Signo::SIGSEGV as i32 | 0x80,
        TimeSpan::from_nanos(11),
        TimeSpan::from_nanos(13),
    );
    assert_eq!(dumped.code(), linux_raw_sys::general::CLD_DUMPED as i32);
    assert_eq!(dumped.status(), Signo::SIGSEGV as i32);
}

#[def_test]
fn test_signal_info_child_exit_payload() {
    let child = ChildExitInfo::from_wait_status(
        42,
        1000,
        7 << 8,
        TimeSpan::from_millis(20),
        TimeSpan::from_millis(30),
    );
    let sigchld = ChildExitSignalInfo::new_sigchld(child);
    let info = sigchld.as_child_exit_signal().as_signal_info();
    let payload = info.child_exit().expect("SIGCHLD must carry child payload");

    assert_eq!(info.signo(), Signo::SIGCHLD);
    assert_eq!(info.code(), linux_raw_sys::general::CLD_EXITED as i32);
    assert_eq!(payload.pid(), 42);
    assert_eq!(payload.uid(), 1000);
    assert_eq!(payload.status(), 7);
    assert_eq!(payload.utime(), TimeSpan::from_millis(20));
    assert_eq!(payload.stime(), TimeSpan::from_millis(30));
}

fn new_sigchld_child_exit_signal() -> crate::SigchldChildExitSignalInfo {
    let child = ChildExitInfo::from_wait_status(
        42,
        1000,
        7 << 8,
        TimeSpan::from_millis(20),
        TimeSpan::from_millis(30),
    );
    ChildExitSignalInfo::new_sigchld(child)
}

fn drop_dequeued_signal(sig: &SignalInfo) -> SignalDequeueAction {
    LAST_DEQUEUED_SIGNAL.store(sig.signo() as usize, Ordering::Relaxed);
    DEQUEUED_SIGNAL_COUNT.fetch_add(1, Ordering::Relaxed);
    SignalDequeueAction::Drop
}

#[def_test]
fn test_signo_properties() {
    assert_eq!(Signo::SIGHUP as i32, 1);
    assert_eq!(Signo::SIGRTMIN as i32, 32);

    assert!(!Signo::SIGINT.is_realtime());
    assert!(Signo::SIGRTMIN.is_realtime());
    assert!(Signo::SIGRT32.is_realtime());

    assert_eq!(
        Signo::SIGINT.default_action(),
        DefaultSignalAction::Terminate
    );
    assert_eq!(Signo::SIGCHLD.default_action(), DefaultSignalAction::Ignore);
}

#[def_test]
fn test_signal_set() {
    let mut set = SignalSet::default();
    assert!(set.is_empty());

    assert!(set.add(Signo::SIGINT));
    assert!(set.has(Signo::SIGINT));
    assert!(!set.is_empty());

    // Adding same signal again returns false
    assert!(!set.add(Signo::SIGINT));

    assert!(set.remove(Signo::SIGINT));
    assert!(!set.has(Signo::SIGINT));
    assert!(set.is_empty());

    // Removing non-existent signal returns false
    assert!(!set.remove(Signo::SIGINT));
}

#[def_test]
fn test_signal_set_dequeuing() {
    let mut set = SignalSet::default();
    set.add(Signo::SIGINT); // 2
    set.add(Signo::SIGKILL); // 9
    set.add(Signo::SIGUSR1); // 10

    let mut mask = SignalSet::default();
    mask.add(Signo::SIGINT);
    mask.add(Signo::SIGUSR1);

    // Should dequeue priority order (lowest number first usually, based on implementation)
    // implementation uses trailing_zeros, so lowest bit -> lowest signal number.
    let dequeued = set.dequeue(&mask);
    assert_eq!(dequeued, Some(Signo::SIGINT));
    assert!(!set.has(Signo::SIGINT));

    let dequeued = set.dequeue(&mask);
    assert_eq!(dequeued, Some(Signo::SIGUSR1));

    let dequeued = set.dequeue(&mask);
    assert_eq!(dequeued, None);

    // SIGKILL should still be there
    assert!(set.has(Signo::SIGKILL));
}

#[def_test]
fn test_pending_signals_std() {
    let mut pending = PendingSignals::default();
    let siginfo_int = SignalInfo::new_kernel(Signo::SIGINT);
    let siginfo_kill = SignalInfo::new_kernel(Signo::SIGKILL);

    assert!(pending.put_signal(siginfo_int.clone()).unwrap());
    assert!(pending.set.has(Signo::SIGINT));

    // Put duplicate std signal -> should return false
    assert!(!pending.put_signal(siginfo_int.clone()).unwrap());

    assert!(pending.put_signal(siginfo_kill).unwrap());
    assert!(pending.set.has(Signo::SIGKILL));

    // Dequeue
    let mut mask = SignalSet::default();
    mask.add(Signo::SIGINT);
    let dequeued = pending.dequeue_signal(&mask);
    assert!(dequeued.is_some());
    assert_eq!(dequeued.unwrap().signo(), Signo::SIGINT);
    assert!(!pending.set.has(Signo::SIGINT));
}

#[def_test]
fn test_pending_signals_rt() {
    let mut pending = PendingSignals::default();
    let rt1 = Signo::SIGRTMIN;
    let info1 = SignalInfo::new_user(rt1, 0, 100);
    let info2 = SignalInfo::new_user(rt1, 0, 101);

    // RT signals allow multiple instances
    assert!(pending.put_signal(info1).unwrap());
    assert!(pending.put_signal(info2).unwrap());
    assert!(pending.set.has(rt1));

    let mut mask = SignalSet::default();
    mask.add(rt1);

    let d1 = pending.dequeue_signal(&mask);
    assert!(d1.is_some());
    // Verify FIFO? Implementation uses push_back and pop_front.
    // So info1 should come out first.
    // Need to verify unique property of info1 vs info2.
    // SignalInfo internals access is tricky, but let's assume order.

    // After first dequeue, rt1 bit should STILL be set because info2 is there.
    assert!(pending.set.has(rt1));

    let d2 = pending.dequeue_signal(&mask);
    assert!(d2.is_some());

    // Now it should be empty
    assert!(!pending.set.has(rt1));

    let d3 = pending.dequeue_signal(&mask);
    assert!(d3.is_none());
}

#[def_test]
fn test_signo_default_action_full() {
    use DefaultSignalAction::*;
    let expected: &[(Signo, DefaultSignalAction)] = &[
        (Signo::SIGHUP, Terminate),
        (Signo::SIGINT, Terminate),
        (Signo::SIGQUIT, CoreDump),
        (Signo::SIGILL, CoreDump),
        (Signo::SIGTRAP, CoreDump),
        (Signo::SIGABRT, CoreDump),
        (Signo::SIGBUS, CoreDump),
        (Signo::SIGFPE, CoreDump),
        (Signo::SIGKILL, Terminate),
        (Signo::SIGUSR1, Terminate),
        (Signo::SIGSEGV, CoreDump),
        (Signo::SIGUSR2, Terminate),
        (Signo::SIGPIPE, Terminate),
        (Signo::SIGALRM, Terminate),
        (Signo::SIGTERM, Terminate),
        (Signo::SIGSTKFLT, Terminate),
        (Signo::SIGCHLD, Ignore),
        (Signo::SIGCONT, Continue),
        (Signo::SIGSTOP, Stop),
        (Signo::SIGTSTP, Stop),
        (Signo::SIGTTIN, Stop),
        (Signo::SIGTTOU, Stop),
        (Signo::SIGURG, Ignore),
        (Signo::SIGXCPU, CoreDump),
        (Signo::SIGXFSZ, CoreDump),
        (Signo::SIGVTALRM, Terminate),
        (Signo::SIGPROF, Terminate),
        (Signo::SIGWINCH, Ignore),
        (Signo::SIGIO, Terminate),
        (Signo::SIGPWR, Terminate),
        (Signo::SIGSYS, CoreDump),
        (Signo::SIGRTMIN, Terminate),
    ];
    for &(sig, action) in expected {
        assert_eq!(sig.default_action(), action);
    }
}

#[def_test(serial)]
fn test_signal_dequeue_observer_per_signo() {
    crate::unregister_signal_observer(Signo::SIGUSR1);
    crate::unregister_signal_observer(Signo::SIGUSR2);
    crate::register_signal_observer(Signo::SIGUSR1, record_dequeued_signal);
    crate::register_signal_observer(Signo::SIGUSR2, record_dequeued_signal);
    DEQUEUED_SIGNAL_COUNT.store(0, Ordering::Relaxed);
    LAST_DEQUEUED_SIGNAL.store(0, Ordering::Relaxed);

    let thread = new_thread_manager();
    let mut mask = SignalSet::default();
    mask.add(Signo::SIGUSR1);
    mask.add(Signo::SIGUSR2);
    mask.add(Signo::SIGINT);

    // SIGUSR1 — registered, observer fires.
    assert!(
        thread
            .send_signal(SignalInfo::new_kernel(Signo::SIGUSR1))
            .unwrap()
    );
    let signal = thread.dequeue_signal(&mask).unwrap();
    assert_eq!(signal.signo(), Signo::SIGUSR1);
    assert_eq!(DEQUEUED_SIGNAL_COUNT.load(Ordering::Relaxed), 1);

    // SIGUSR2 via process — registered, observer fires.
    assert_eq!(
        thread
            .process()
            .send_signal(SignalInfo::new_kernel(Signo::SIGUSR2))
            .unwrap(),
        Some(1)
    );
    let signal = thread.dequeue_signal(&mask).unwrap();
    assert_eq!(signal.signo(), Signo::SIGUSR2);
    assert_eq!(DEQUEUED_SIGNAL_COUNT.load(Ordering::Relaxed), 2);

    // SIGINT — NOT registered, observer must NOT fire.
    assert!(
        thread
            .send_signal(SignalInfo::new_kernel(Signo::SIGINT))
            .unwrap()
    );
    let signal = thread.dequeue_signal(&mask).unwrap();
    assert_eq!(signal.signo(), Signo::SIGINT);
    assert_eq!(DEQUEUED_SIGNAL_COUNT.load(Ordering::Relaxed), 2);

    crate::unregister_signal_observer(Signo::SIGUSR1);
    crate::unregister_signal_observer(Signo::SIGUSR2);
}

#[def_test(serial)]
fn test_signal_dequeue_observer_can_drop_signal() {
    crate::unregister_signal_observer(Signo::SIGUSR1);
    crate::register_signal_observer(Signo::SIGUSR1, drop_dequeued_signal);
    DEQUEUED_SIGNAL_COUNT.store(0, Ordering::Relaxed);
    LAST_DEQUEUED_SIGNAL.store(0, Ordering::Relaxed);

    let thread = new_thread_manager();
    let mut mask = SignalSet::default();
    mask.add(Signo::SIGUSR1);
    mask.add(Signo::SIGUSR2);

    assert!(
        thread
            .send_signal(SignalInfo::new_kernel(Signo::SIGUSR2))
            .unwrap()
    );
    assert!(
        thread
            .send_signal(SignalInfo::new_kernel(Signo::SIGUSR1))
            .unwrap()
    );

    let signal = thread.dequeue_signal(&mask).unwrap();
    assert_eq!(signal.signo(), Signo::SIGUSR2);
    assert_eq!(DEQUEUED_SIGNAL_COUNT.load(Ordering::Relaxed), 1);
    assert_eq!(
        LAST_DEQUEUED_SIGNAL.load(Ordering::Relaxed),
        Signo::SIGUSR1 as usize
    );

    crate::unregister_signal_observer(Signo::SIGUSR1);
}

#[def_test]
fn test_signal_info_code_and_errno() {
    let info = SignalInfo::new_kernel(Signo::SIGINT);
    assert_eq!(info.signo(), Signo::SIGINT);
    assert_eq!(info.code(), linux_raw_sys::general::SI_KERNEL as i32);
    assert_eq!(info.errno(), 0);

    let user_info = SignalInfo::new_user(Signo::SIGUSR1, 42, 1234);
    assert_eq!(user_info.signo(), Signo::SIGUSR1);
    assert_eq!(user_info.code(), 42);
}

#[def_test]
fn test_signal_info_debug() {
    let info = SignalInfo::new_kernel(Signo::SIGTERM);
    let dbg = format!("{:?}", info);
    assert!(dbg.contains("SignalInfo"));
    assert!(dbg.contains("SIGTERM"));
}

#[def_test]
fn test_signal_set_bitwise_ops() {
    let mut a = SignalSet::default();
    a.add(Signo::SIGINT);
    a.add(Signo::SIGKILL);

    let mut b = SignalSet::default();
    b.add(Signo::SIGKILL);
    b.add(Signo::SIGUSR1);

    let or_set = a | b;
    assert!(or_set.has(Signo::SIGINT));
    assert!(or_set.has(Signo::SIGKILL));
    assert!(or_set.has(Signo::SIGUSR1));

    let and_set = a & b;
    assert!(!and_set.has(Signo::SIGINT));
    assert!(and_set.has(Signo::SIGKILL));
    assert!(!and_set.has(Signo::SIGUSR1));

    let not_a = !a;
    assert!(!not_a.has(Signo::SIGINT));
    assert!(!not_a.has(Signo::SIGKILL));
    assert!(not_a.has(Signo::SIGUSR1));
}

#[def_test]
fn test_signal_set_assign_ops() {
    let mut a = SignalSet::default();
    a.add(Signo::SIGINT);

    let mut b = SignalSet::default();
    b.add(Signo::SIGKILL);

    a |= b;
    assert!(a.has(Signo::SIGINT));
    assert!(a.has(Signo::SIGKILL));

    a &= b;
    assert!(!a.has(Signo::SIGINT));
    assert!(a.has(Signo::SIGKILL));
}

#[def_test]
fn test_signal_set_debug() {
    let mut set = SignalSet::default();
    set.add(Signo::SIGINT);
    set.add(Signo::SIGTERM);
    let dbg = format!("{:?}", set);
    assert!(dbg.contains("SIGINT"));
    assert!(dbg.contains("SIGTERM"));
}

#[def_test]
fn test_signal_set_kernel_sigset_roundtrip() {
    let mut set = SignalSet::default();
    set.add(Signo::SIGINT);
    set.add(Signo::SIGKILL);
    set.add(Signo::SIGRTMIN);

    let kernel: linux_raw_sys::general::kernel_sigset_t = set.into();
    let restored = SignalSet::from(kernel);
    assert!(restored.has(Signo::SIGINT));
    assert!(restored.has(Signo::SIGKILL));
    assert!(restored.has(Signo::SIGRTMIN));
    assert!(!restored.has(Signo::SIGUSR1));
}

#[def_test]
fn test_signal_stack_default_disabled() {
    let stack = SignalStack::default();
    assert_eq!(stack.sp, 0);
    assert_eq!(stack.size, 0);
    assert!(stack.disabled());

    let active_stack = SignalStack {
        sp: 0x1000,
        flags: 0,
        size: 4096,
    };
    assert!(!active_stack.disabled());
}

#[def_test]
fn test_signal_action_abi_roundtrip() {
    extern "C" fn handler(_: i32) {}

    let mut mask = SignalSet::default();
    mask.add(Signo::SIGUSR2);
    for disposition in [
        SignalDisposition::Default,
        SignalDisposition::Ignore,
        SignalDisposition::Handler(handler),
    ] {
        let raw = posix_types::k_sigaction::from(SignalAction {
            flags: SignalActionFlags::RESTART,
            mask,
            disposition,
            restorer: None,
        });
        let restored = posix_types::k_sigaction::from(SignalAction::from(raw));
        assert_eq!(
            restored.sa_handler_kernel.map(|f| f as usize),
            raw.sa_handler_kernel.map(|f| f as usize)
        );
        assert_eq!(restored.sa_flags, raw.sa_flags);
        assert_eq!(restored.sa_mask.sig, raw.sa_mask.sig);
    }
}

#[def_test]
fn test_signal_action_default() {
    let action = SignalAction::default();
    assert!(matches!(
        action.disposition,
        crate::SignalDisposition::Default
    ));
    assert!(action.mask.is_empty());
}

/// Drives the availability projection through every transition the fast path
/// depends on, for both queues.
///
/// This is the regression guard for `F-04` (a projection that under-reports
/// strands a deliverable signal until an unrelated event re-arms the check), so
/// it asserts the projection cell itself rather than delivery behaviour.
#[def_test]
fn test_availability_projection_tracks_both_queues() {
    fn block(lock: &crate::SigLock, slot: crate::SlotId, signo: Signo) {
        let mut set = SignalSet::default();
        set.add(signo);
        lock.transact(|txn| txn.set_blocked(slot, set));
    }
    fn unblock(lock: &crate::SigLock, slot: crate::SlotId) {
        lock.transact(|txn| txn.set_blocked(slot, SignalSet::default()));
    }

    let lock = crate::SigLock::new();
    let (slot, hint) = lock.register(11, kalloc_types::AllocFlags::KERNEL).unwrap();
    assert!(!hint.load(Ordering::Acquire));

    // Thread-private queue: enqueue, mask, unmask, dequeue.
    let _rejected = lock.transact(|txn| {
        txn.put_thread(slot, SignalInfo::new_kernel(Signo::SIGTERM))
            .unwrap()
    });
    assert!(hint.load(Ordering::Acquire));
    block(&lock, slot, Signo::SIGTERM);
    assert!(!hint.load(Ordering::Acquire));
    unblock(&lock, slot);
    assert!(hint.load(Ordering::Acquire));
    let taken = lock
        .transact(|txn| txn.take(slot))
        .and_then(|detached| detached.complete())
        .map(|dequeued| dequeued.sig.signo());
    assert_eq!(taken, Some(Signo::SIGTERM));
    assert!(!hint.load(Ordering::Acquire));

    // Process-directed queue: the same transitions, but the answer depends on
    // this thread's mask, so the queueing side must publish every slot.
    let _rejected = lock.transact(|txn| {
        txn.put_shared(SignalInfo::new_kernel(Signo::SIGUSR1))
            .unwrap()
    });
    assert!(hint.load(Ordering::Acquire));
    block(&lock, slot, Signo::SIGUSR1);
    assert!(!hint.load(Ordering::Acquire));
    unblock(&lock, slot);
    assert!(hint.load(Ordering::Acquire));
    let taken = lock
        .transact(|txn| txn.take(slot))
        .and_then(|detached| detached.complete())
        .map(|dequeued| dequeued.sig.signo());
    assert_eq!(taken, Some(Signo::SIGUSR1));
    assert!(!hint.load(Ordering::Acquire));
}

/// The identity mechanism that replaced the `Weak` registry: `leave_group`
/// takes the thread out of delivery-target selection.
#[def_test]
fn test_leave_group_removes_delivery_target() {
    let lock = crate::SigLock::new();
    let (first, _) = lock.register(21, kalloc_types::AllocFlags::KERNEL).unwrap();
    let (second, _) = lock.register(22, kalloc_types::AllocFlags::KERNEL).unwrap();

    let (target, blocked) = lock.inspect(|view| view.find_target(Signo::SIGUSR1));
    assert_eq!(target, Some(21));
    assert!(!blocked);

    // A target that blocks the signal is skipped but still reported, so the
    // "ignored signals stay pending while someone blocks them" policy holds.
    let mut set = SignalSet::default();
    set.add(Signo::SIGUSR1);
    lock.transact(|txn| txn.set_blocked(first, set));
    let (target, blocked) = lock.inspect(|view| view.find_target(Signo::SIGUSR1));
    assert_eq!(target, Some(22));
    assert!(blocked);

    // Leaving the group removes the thread from selection entirely, including
    // from the blocked report.
    lock.transact(|txn| txn.leave_group(second));
    let (target, blocked) = lock.inspect(|view| view.find_target(Signo::SIGUSR1));
    assert_eq!(target, None);
    assert!(blocked);

    lock.transact(|txn| txn.leave_group(first));
    let (target, blocked) = lock.inspect(|view| view.find_target(Signo::SIGUSR1));
    assert_eq!(target, None);
    assert!(!blocked);
}

/// A retired slot handle must never reach the successor that reuses its index.
#[def_test]
fn test_retired_slot_handle_cannot_address_reused_slot() {
    let lock = crate::SigLock::new();
    let (stale, _) = lock.register(31, kalloc_types::AllocFlags::KERNEL).unwrap();
    lock.retire(stale);

    // The freed index is reused, with a new generation.
    let (successor, hint) = lock.register(32, kalloc_types::AllocFlags::KERNEL).unwrap();
    assert!(!hint.load(Ordering::Acquire));

    let mut set = SignalSet::default();
    set.add(Signo::SIGTERM);
    lock.transact(|txn| txn.set_blocked(stale, set));
    assert!(lock.inspect(|view| view.blocked(successor)).is_empty());

    assert!(
        lock.transact(|txn| txn
            .put_thread(stale, SignalInfo::new_kernel(Signo::SIGTERM))
            .unwrap())
            .is_some()
    );
    assert!(lock.inspect(|view| view.pending(successor)).is_empty());
    assert!(lock.transact(|txn| txn.take(stale)).is_none());
    assert!(!hint.load(Ordering::Acquire));

    // The successor's own handle still works.
    assert!(
        lock.transact(|txn| txn
            .put_thread(successor, SignalInfo::new_kernel(Signo::SIGTERM))
            .unwrap())
            .is_none()
    );
    assert!(
        lock.inspect(|view| view.pending(successor))
            .has(Signo::SIGTERM)
    );
    assert!(hint.load(Ordering::Acquire));
}

#[def_test]
fn test_blocked_write_entry_drops_unblockable_signals() {
    let lock = crate::SigLock::new();
    let (slot, _hint) = lock.register(7, kalloc_types::AllocFlags::KERNEL).unwrap();

    let mut named = SignalSet::default();
    named.add(Signo::SIGTERM);
    named.add(Signo::SIGKILL);
    named.add(Signo::SIGSTOP);

    let (after_set, after_add) = lock.transact(|txn| {
        txn.set_blocked(slot, named);
        let after_set = txn.view().blocked(slot);
        txn.add_blocked(slot, named);
        (after_set, txn.view().blocked(slot))
    });

    // Both writers are the same entry, so a handler mask cannot reintroduce
    // an unblockable signal either.
    assert!(after_set.has(Signo::SIGTERM));
    assert!(!after_set.has(Signo::SIGKILL));
    assert!(!after_set.has(Signo::SIGSTOP));
    assert!(after_add.has(Signo::SIGTERM));
    assert!(!after_add.has(Signo::SIGKILL));
    assert!(!after_add.has(Signo::SIGSTOP));
}

#[def_test]
fn test_thread_signal_manager_blocked_and_stack_helpers() {
    let thread = new_thread_manager();

    let mut set = SignalSet::default();
    set.add(Signo::SIGTERM);
    set.add(Signo::SIGKILL);
    set.add(Signo::SIGSTOP);

    let old = thread.set_blocked(set);
    assert!(old.is_empty());
    assert!(thread.blocked().has(Signo::SIGTERM));
    assert!(!thread.blocked().has(Signo::SIGKILL));
    assert!(!thread.blocked().has(Signo::SIGSTOP));

    let stack = SignalStack {
        sp: 0x4000,
        flags: 0,
        size: 8192,
    };
    thread.set_stack(stack.clone());
    let got = thread.stack();
    assert_eq!(got.sp, stack.sp);
    assert_eq!(got.flags, stack.flags);
    assert_eq!(got.size, stack.size);
}

#[def_test]
fn test_thread_signal_manager_saved_sigmask_helpers() {
    let thread = new_thread_manager();

    let mut saved = SignalSet::default();
    saved.add(Signo::SIGUSR1);
    thread.set_saved_sigmask(saved);

    let restored = thread.take_saved_sigmask();
    assert!(restored.is_some());
    assert!(restored.unwrap().has(Signo::SIGUSR1));
    assert!(thread.take_saved_sigmask().is_none());
}

#[def_test]
fn test_thread_signal_manager_send_signal_and_pending_merge() {
    let thread = new_thread_manager();

    assert!(
        thread
            .send_signal(SignalInfo::new_kernel(Signo::SIGTERM))
            .unwrap()
    );
    assert!(thread.pending().has(Signo::SIGTERM));

    let target = thread
        .process()
        .send_signal(SignalInfo::new_kernel(Signo::SIGUSR1))
        .unwrap();
    assert_eq!(target, Some(1));
    let pending = thread.pending();
    assert!(pending.has(Signo::SIGTERM));
    assert!(pending.has(Signo::SIGUSR1));
}

#[def_test]
fn test_thread_signal_manager_send_signal_ignored_and_blocked_paths() {
    let thread = new_thread_manager();

    thread
        .process()
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
            .send_signal(SignalInfo::new_kernel(Signo::SIGUSR1))
            .unwrap()
    );
    assert!(!thread.pending().has(Signo::SIGUSR1));

    let mut set = SignalSet::default();
    set.add(Signo::SIGUSR1);
    set.add(Signo::SIGTERM);
    thread.set_blocked(set);

    assert!(
        !thread
            .send_signal(SignalInfo::new_kernel(Signo::SIGUSR1))
            .unwrap()
    );
    assert!(thread.pending().has(Signo::SIGUSR1));

    assert!(
        !thread
            .send_signal(SignalInfo::new_kernel(Signo::SIGTERM))
            .unwrap()
    );
    assert!(thread.pending().has(Signo::SIGTERM));
}

#[def_test]
fn test_send_signal_default_continue_is_implicitly_ignored() {
    let thread = new_thread_manager();

    // A default-disposition SIGCONT acts as an implicit ignore (Linux
    // SIG_KERNEL_IGNORE_MASK): it must be dropped instead of queued, so it
    // cannot wake interruptible waiters out of nanosleep- or poll-style waits.
    assert!(
        !thread
            .send_signal(SignalInfo::new_kernel(Signo::SIGCONT))
            .unwrap()
    );
    assert!(!thread.pending().has(Signo::SIGCONT));
    assert_eq!(
        thread
            .process()
            .send_signal(SignalInfo::new_kernel(Signo::SIGCONT))
            .unwrap(),
        None
    );
    assert!(!thread.process().pending().has(Signo::SIGCONT));

    // A blocked SIGCONT still becomes pending: userspace may install a
    // handler before unblocking it.
    let mut set = SignalSet::default();
    set.add(Signo::SIGCONT);
    thread.set_blocked(set);

    assert!(
        !thread
            .send_signal(SignalInfo::new_kernel(Signo::SIGCONT))
            .unwrap()
    );
    assert!(thread.pending().has(Signo::SIGCONT));
}

#[def_test]
fn test_process_signal_manager_can_restart_and_signal_ignored() {
    let thread = new_thread_manager();
    let proc = thread.process();

    assert!(!proc.can_restart(Signo::SIGTERM).unwrap());
    let mut action = proc.signal_action(Signo::SIGTERM).unwrap();
    action.flags |= SignalActionFlags::RESTART;
    proc.set_signal_action(Signo::SIGTERM, action.clone())
        .unwrap();
    assert!(proc.can_restart(Signo::SIGTERM).unwrap());

    assert!(proc.signal_ignored(Signo::SIGCHLD).unwrap());
    action.disposition = SignalDisposition::Ignore;
    proc.set_signal_action(Signo::SIGTERM, action).unwrap();
    assert!(proc.signal_ignored(Signo::SIGTERM).unwrap());
}

#[def_test]
fn test_process_signal_manager_pending_and_ignored_send() {
    let thread = new_thread_manager();
    let proc = thread.process();

    proc.set_signal_action(
        Signo::SIGUSR2,
        SignalAction {
            disposition: SignalDisposition::Ignore,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        proc.send_signal(SignalInfo::new_kernel(Signo::SIGUSR2))
            .unwrap(),
        None
    );
    assert!(!proc.pending().has(Signo::SIGUSR2));

    let mut set = SignalSet::default();
    set.add(Signo::SIGCHLD);
    set.add(Signo::SIGUSR2);
    thread.set_blocked(set);

    assert_eq!(
        proc.send_signal(SignalInfo::new_kernel(Signo::SIGCHLD))
            .unwrap(),
        None
    );
    assert!(proc.pending().has(Signo::SIGCHLD));

    assert_eq!(
        proc.send_signal(SignalInfo::new_kernel(Signo::SIGUSR2))
            .unwrap(),
        None
    );
    assert!(proc.pending().has(Signo::SIGUSR2));

    assert_eq!(
        proc.send_signal(SignalInfo::new_kernel(Signo::SIGINT))
            .unwrap(),
        Some(1)
    );
    assert!(proc.pending().has(Signo::SIGINT));
}

#[def_test]
fn test_process_signal_manager_child_exit_sigchld_uses_linux_default_semantics() {
    let thread = new_thread_manager();
    let proc = thread.process();

    assert!(proc.signal_ignored(Signo::SIGCHLD).unwrap());
    let prepared = proc
        .prepare_child_exit_signal(new_sigchld_child_exit_signal())
        .unwrap();
    assert!(!prepared.should_autoreap());
    assert!(
        !proc.pending().has(Signo::SIGCHLD),
        "preparing child-exit delivery must not publish pending SIGCHLD"
    );
    assert_eq!(
        proc.commit_child_exit_signal(prepared),
        Some(1),
        "commit must choose the currently unblocked target thread"
    );
    assert!(proc.pending().has(Signo::SIGCHLD));

    let mut sigchld = SignalSet::default();
    sigchld.add(Signo::SIGCHLD);
    assert!(thread.dequeue_signal(&sigchld).is_some());

    let (result, target_tid) = proc
        .send_child_exit_signal(new_sigchld_child_exit_signal())
        .unwrap();
    assert_eq!(target_tid, Some(1));
    assert!(!result.should_autoreap());
    assert!(proc.pending().has(Signo::SIGCHLD));

    assert!(thread.dequeue_signal(&sigchld).is_some());
    proc.set_signal_action(
        Signo::SIGCHLD,
        SignalAction {
            disposition: SignalDisposition::Ignore,
            ..Default::default()
        },
    )
    .unwrap();
    let (result, target_tid) = proc
        .send_child_exit_signal(new_sigchld_child_exit_signal())
        .unwrap();
    assert_eq!(target_tid, None);
    assert!(result.should_autoreap());
    assert!(!proc.pending().has(Signo::SIGCHLD));

    proc.set_signal_action(
        Signo::SIGCHLD,
        SignalAction {
            flags: SignalActionFlags::NOCLDWAIT,
            ..Default::default()
        },
    )
    .unwrap();
    let (result, target_tid) = proc
        .send_child_exit_signal(new_sigchld_child_exit_signal())
        .unwrap();
    assert_eq!(target_tid, Some(1));
    assert!(result.should_autoreap());
    assert!(proc.pending().has(Signo::SIGCHLD));
}

#[def_test]
fn test_child_exit_sigchld_commit_reselects_target_after_mask_change() {
    let thread = new_thread_manager();
    let proc = thread.process();
    let prepared = proc
        .prepare_child_exit_signal(new_sigchld_child_exit_signal())
        .unwrap();

    let mut blocked = SignalSet::default();
    blocked.add(Signo::SIGCHLD);
    thread.set_blocked(blocked);

    assert_eq!(
        proc.commit_child_exit_signal(prepared),
        None,
        "commit must not reuse a target selected before the signal mask changed"
    );
    assert!(
        proc.pending().has(Signo::SIGCHLD),
        "default SIGCHLD should still be queued even when currently blocked"
    );
}

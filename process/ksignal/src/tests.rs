// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unit tests for ksignal

#![cfg(unittest)]

use alloc::format;
use core::sync::atomic::{AtomicUsize, Ordering};

use kspin::SpinNoIrq;
use unittest::{assert, assert_eq, def_test};

use crate::{
    ChildExitInfo, ChildExitSignalInfo, DefaultSignalAction, PendingSignals, SignalAction,
    SignalActionFlags, SignalDequeueAction, SignalDisposition, SignalInfo, SignalSet, SignalStack,
    Signo,
    api::{ProcessSignalManager, SignalActions, ThreadSignalManager},
};

fn new_thread_manager() -> alloc::sync::Arc<ThreadSignalManager> {
    let actions = alloc::sync::Arc::new(SpinNoIrq::new(SignalActions::default()));
    let proc = alloc::sync::Arc::new(ProcessSignalManager::new(actions, 0));
    ThreadSignalManager::new(1, proc)
}

static DEQUEUED_SIGNAL_COUNT: AtomicUsize = AtomicUsize::new(0);
static LAST_DEQUEUED_SIGNAL: AtomicUsize = AtomicUsize::new(0);

fn record_dequeued_signal(sig: &SignalInfo) -> SignalDequeueAction {
    LAST_DEQUEUED_SIGNAL.store(sig.signo() as usize, Ordering::Relaxed);
    DEQUEUED_SIGNAL_COUNT.fetch_add(1, Ordering::Relaxed);
    SignalDequeueAction::Deliver
}

#[def_test]
fn test_child_exit_info_decodes_wait_status() {
    let exited = ChildExitInfo::from_wait_status(42, 1000, 7 << 8, 11, 13);
    assert_eq!(exited.code(), linux_raw_sys::general::CLD_EXITED as i32);
    assert_eq!(exited.status(), 7);

    let killed = ChildExitInfo::from_wait_status(42, 1000, Signo::SIGTERM as i32, 11, 13);
    assert_eq!(killed.code(), linux_raw_sys::general::CLD_KILLED as i32);
    assert_eq!(killed.status(), Signo::SIGTERM as i32);

    let dumped = ChildExitInfo::from_wait_status(42, 1000, Signo::SIGSEGV as i32 | 0x80, 11, 13);
    assert_eq!(dumped.code(), linux_raw_sys::general::CLD_DUMPED as i32);
    assert_eq!(dumped.status(), Signo::SIGSEGV as i32);
}

#[def_test]
fn test_signal_info_child_exit_payload() {
    let child = ChildExitInfo::from_wait_status(42, 1000, 7 << 8, 11, 13);
    let sigchld = ChildExitSignalInfo::new_sigchld(child);
    let info = sigchld.as_child_exit_signal().as_signal_info();
    let payload = info.child_exit().expect("SIGCHLD must carry child payload");

    assert_eq!(info.signo(), Signo::SIGCHLD);
    assert_eq!(info.code(), linux_raw_sys::general::CLD_EXITED as i32);
    assert_eq!(payload.pid(), 42);
    assert_eq!(payload.uid(), 1000);
    assert_eq!(payload.status(), 7);
    assert_eq!(payload.utime_ticks(), 11);
    assert_eq!(payload.stime_ticks(), 13);
}

fn new_sigchld_child_exit_signal() -> crate::SigchldChildExitSignalInfo {
    let child = ChildExitInfo::from_wait_status(42, 1000, 7 << 8, 11, 13);
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

    assert!(pending.put_signal(siginfo_int.clone()));
    assert!(pending.set.has(Signo::SIGINT));

    // Put duplicate std signal -> should return false
    assert!(!pending.put_signal(siginfo_int.clone()));

    assert!(pending.put_signal(siginfo_kill));
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
    assert!(pending.put_signal(info1));
    assert!(pending.put_signal(info2));
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
    assert!(thread.send_signal(SignalInfo::new_kernel(Signo::SIGUSR1)));
    let signal = thread.dequeue_signal(&mask).unwrap();
    assert_eq!(signal.signo(), Signo::SIGUSR1);
    assert_eq!(DEQUEUED_SIGNAL_COUNT.load(Ordering::Relaxed), 1);

    // SIGUSR2 via process — registered, observer fires.
    assert_eq!(
        thread
            .process()
            .send_signal(SignalInfo::new_kernel(Signo::SIGUSR2)),
        Some(1)
    );
    let signal = thread.dequeue_signal(&mask).unwrap();
    assert_eq!(signal.signo(), Signo::SIGUSR2);
    assert_eq!(DEQUEUED_SIGNAL_COUNT.load(Ordering::Relaxed), 2);

    // SIGINT — NOT registered, observer must NOT fire.
    assert!(thread.send_signal(SignalInfo::new_kernel(Signo::SIGINT)));
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

    assert!(thread.send_signal(SignalInfo::new_kernel(Signo::SIGUSR2)));
    assert!(thread.send_signal(SignalInfo::new_kernel(Signo::SIGUSR1)));

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
fn test_signal_action_default() {
    let action = SignalAction::default();
    assert!(matches!(
        action.disposition,
        crate::SignalDisposition::Default
    ));
    assert!(action.mask.is_empty());
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
    assert!(thread.signal_blocked(Signo::SIGTERM));
    assert!(!thread.signal_blocked(Signo::SIGKILL));
    assert!(!thread.signal_blocked(Signo::SIGSTOP));

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

    assert!(thread.send_signal(SignalInfo::new_kernel(Signo::SIGTERM)));
    assert!(thread.pending().has(Signo::SIGTERM));

    let target = thread
        .process()
        .send_signal(SignalInfo::new_kernel(Signo::SIGUSR1));
    assert_eq!(target, Some(1));
    let pending = thread.pending();
    assert!(pending.has(Signo::SIGTERM));
    assert!(pending.has(Signo::SIGUSR1));
}

#[def_test]
fn test_thread_signal_manager_send_signal_ignored_and_blocked_paths() {
    let thread = new_thread_manager();

    thread.process().actions.lock()[Signo::SIGUSR1].disposition = SignalDisposition::Ignore;
    assert!(!thread.send_signal(SignalInfo::new_kernel(Signo::SIGUSR1)));
    assert!(!thread.pending().has(Signo::SIGUSR1));

    let mut set = SignalSet::default();
    set.add(Signo::SIGUSR1);
    set.add(Signo::SIGTERM);
    thread.set_blocked(set);

    assert!(!thread.send_signal(SignalInfo::new_kernel(Signo::SIGUSR1)));
    assert!(thread.pending().has(Signo::SIGUSR1));

    assert!(!thread.send_signal(SignalInfo::new_kernel(Signo::SIGTERM)));
    assert!(thread.pending().has(Signo::SIGTERM));
}

#[def_test]
fn test_process_signal_manager_can_restart_and_signal_ignored() {
    let thread = new_thread_manager();
    let proc = thread.process();

    assert!(!proc.can_restart(Signo::SIGTERM));
    proc.actions.lock()[Signo::SIGTERM].flags |= SignalActionFlags::RESTART;
    assert!(proc.can_restart(Signo::SIGTERM));

    assert!(proc.signal_ignored(Signo::SIGCHLD));
    proc.actions.lock()[Signo::SIGTERM].disposition = SignalDisposition::Ignore;
    assert!(proc.signal_ignored(Signo::SIGTERM));
}

#[def_test]
fn test_process_signal_manager_pending_and_ignored_send() {
    let thread = new_thread_manager();
    let proc = thread.process();

    proc.actions.lock()[Signo::SIGUSR2].disposition = SignalDisposition::Ignore;
    assert_eq!(
        proc.send_signal(SignalInfo::new_kernel(Signo::SIGUSR2)),
        None
    );
    assert!(!proc.pending().has(Signo::SIGUSR2));

    let mut set = SignalSet::default();
    set.add(Signo::SIGCHLD);
    set.add(Signo::SIGUSR2);
    thread.set_blocked(set);

    assert_eq!(
        proc.send_signal(SignalInfo::new_kernel(Signo::SIGCHLD)),
        None
    );
    assert!(proc.pending().has(Signo::SIGCHLD));

    assert_eq!(
        proc.send_signal(SignalInfo::new_kernel(Signo::SIGUSR2)),
        None
    );
    assert!(proc.pending().has(Signo::SIGUSR2));

    assert_eq!(
        proc.send_signal(SignalInfo::new_kernel(Signo::SIGINT)),
        Some(1)
    );
    assert!(proc.pending().has(Signo::SIGINT));
}

#[def_test]
fn test_process_signal_manager_child_exit_sigchld_uses_linux_default_semantics() {
    let thread = new_thread_manager();
    let proc = thread.process();

    assert!(proc.signal_ignored(Signo::SIGCHLD));
    let prepared = proc.prepare_child_exit_signal(new_sigchld_child_exit_signal());
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
    let _ = thread.dequeue_signal(&sigchld);

    let (result, target_tid) = proc.send_child_exit_signal(new_sigchld_child_exit_signal());
    assert_eq!(target_tid, Some(1));
    assert!(!result.should_autoreap());
    assert!(proc.pending().has(Signo::SIGCHLD));

    let _ = thread.dequeue_signal(&sigchld);
    proc.actions.lock()[Signo::SIGCHLD].disposition = SignalDisposition::Ignore;
    let (result, target_tid) = proc.send_child_exit_signal(new_sigchld_child_exit_signal());
    assert_eq!(target_tid, None);
    assert!(result.should_autoreap());
    assert!(!proc.pending().has(Signo::SIGCHLD));

    proc.actions.lock()[Signo::SIGCHLD].disposition = SignalDisposition::Default;
    proc.actions.lock()[Signo::SIGCHLD].flags |= SignalActionFlags::NOCLDWAIT;
    let (result, target_tid) = proc.send_child_exit_signal(new_sigchld_child_exit_signal());
    assert_eq!(target_tid, Some(1));
    assert!(result.should_autoreap());
    assert!(proc.pending().has(Signo::SIGCHLD));
}

#[def_test]
fn test_child_exit_sigchld_commit_reselects_target_after_mask_change() {
    let thread = new_thread_manager();
    let proc = thread.process();
    let prepared = proc.prepare_child_exit_signal(new_sigchld_child_exit_signal());

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

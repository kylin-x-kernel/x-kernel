//! Unit tests for ksignal

#![cfg(unittest)]

use unittest::{assert, assert_eq, def_test};

use crate::{DefaultSignalAction, PendingSignals, SignalInfo, SignalSet, Signo};

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

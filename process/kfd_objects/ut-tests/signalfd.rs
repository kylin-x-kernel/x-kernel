// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{
    sync::atomic::{AtomicUsize, Ordering},
    task::{Context, Waker},
};

use kpoll::PollRegistrations;
use ksignal::{
    ChildExitInfo, ChildExitSignalInfo, Signo,
    api::{ProcessSignalManager, SignalActions},
};
use unittest::{assert, assert_eq, def_test};

use super::*;

fn signal_access() -> SignalfdAccess {
    let mut mask = SignalSet::default();
    mask.add(Signo::SIGUSR1);
    let process = Arc::new(ProcessSignalManager::new(SignalActions::default(), 0));
    let signal = ThreadSignalManager::new(1, process);
    signal.set_blocked(mask);
    SignalfdAccess {
        signalfd: Signalfd::new(mask),
        signal,
    }
}

#[def_test]
fn test_signalfd_ready_and_nonblocking_reads_precede_interrupt() {
    let access = signal_access();
    let mut buf = [0; SIGNALFD_SIGINFO_SIZE];
    let task = current();

    // Record results before assertions so the task interrupt cannot leak
    // into the test runner even if a regression makes an assertion fail.
    task.interrupt();
    let empty = block_on(access.read(&mut buf, true));
    let empty_preserved_interrupt = task.is_interrupted();
    task.clear_interrupt();
    assert_eq!(empty, Err(KError::WouldBlock));
    assert!(empty_preserved_interrupt);

    for is_nonblocking in [true, false] {
        assert!(
            !access
                .signal
                .send_signal(SignalInfo::new_kernel(Signo::SIGUSR1))
        );
        task.interrupt();
        let ready = block_on(access.read(&mut buf, is_nonblocking));
        let ready_preserved_interrupt = task.is_interrupted();
        task.clear_interrupt();
        assert_eq!(ready, Ok(SIGNALFD_SIGINFO_SIZE));
        assert!(ready_preserved_interrupt);
        assert!(!access.has_pending_signals());
    }

    task.interrupt();
    let waiting = block_on(access.read(&mut buf, false));
    task.clear_interrupt();
    assert_eq!(waiting, Err(KError::from(LinuxError::ERESTARTSYS)));
}

#[def_test]
fn test_signalfd_record_arriving_during_wait_precedes_interrupt() {
    use core::{
        future::Future,
        pin::pin,
        task::{Context, Poll, Waker},
    };

    let access = signal_access();
    let mut buf = [0; SIGNALFD_SIGINFO_SIZE];
    let mut read = pin!(access.read(&mut buf, false));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(read.as_mut().poll(&mut cx).is_pending());
    assert!(
        !access
            .signal
            .send_signal(SignalInfo::new_kernel(Signo::SIGUSR1))
    );
    let task = current();
    task.interrupt();
    let result = read.as_mut().poll(&mut cx);
    task.clear_interrupt();
    assert_eq!(result, Poll::Ready(Ok(SIGNALFD_SIGINFO_SIZE)));
    assert!(!access.has_pending_signals());
}

struct WakeCount(AtomicUsize);

impl alloc::task::Wake for WakeCount {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[def_test]
fn signalfd_arrival_wakes_blocked_process_and_thread_readers() {
    let process = Arc::new(ProcessSignalManager::new(SignalActions::default(), 0));
    let signal = ThreadSignalManager::new(1, process.clone());
    let mut mask = SignalSet::default();
    mask.add(Signo::SIGURG);
    signal.set_blocked(mask);
    let access = SignalfdAccess {
        signalfd: Signalfd::new(mask),
        signal,
    };
    let count = Arc::new(WakeCount(AtomicUsize::new(0)));
    let waker = Waker::from(count.clone());
    let cx = Context::from_waker(&waker);
    let mut registrations = PollRegistrations::new();
    access
        .register(&mut registrations.context(&cx), IoEvents::IN)
        .unwrap();
    assert!(access.poll().is_empty());
    assert_eq!(
        process.send_signal(SignalInfo::new_kernel(Signo::SIGURG)),
        None
    );
    assert!(count.0.load(Ordering::Relaxed) > 0);
    assert!(access.poll().contains(IoEvents::IN));
    assert_eq!(access.dequeue_signal().unwrap().signo(), Signo::SIGURG);
    assert!(access.poll().is_empty());

    access
        .register(&mut registrations.context(&cx), IoEvents::IN)
        .unwrap();
    count.0.store(0, Ordering::Relaxed);
    assert!(
        !access
            .signal
            .send_signal(SignalInfo::new_kernel(Signo::SIGURG))
    );
    assert!(count.0.load(Ordering::Relaxed) > 0);
    assert_eq!(access.dequeue_signal().unwrap().signo(), Signo::SIGURG);

    access
        .register(&mut registrations.context(&cx), IoEvents::IN)
        .unwrap();
    drop(registrations);
    count.0.store(0, Ordering::Relaxed);
    let _ = process.send_signal(SignalInfo::new_kernel(Signo::SIGURG));
    assert_eq!(count.0.load(Ordering::Relaxed), 0);
}

#[def_test]
fn signalfd_shared_file_keeps_readiness_local_to_accessing_process() {
    let mut mask = SignalSet::default();
    mask.add(Signo::SIGURG);
    let signalfd = Signalfd::new(mask);
    let make_access = || {
        let process = Arc::new(ProcessSignalManager::new(SignalActions::default(), 0));
        let signal = ThreadSignalManager::new(1, process);
        signal.set_blocked(mask);
        SignalfdAccess {
            signalfd: signalfd.clone(),
            signal,
        }
    };
    let first = make_access();
    let second = make_access();
    let count = Arc::new(WakeCount(AtomicUsize::new(0)));
    let waker = Waker::from(count.clone());
    let cx = Context::from_waker(&waker);
    let mut registrations = PollRegistrations::new();
    second
        .register(&mut registrations.context(&cx), IoEvents::IN)
        .unwrap();
    let _ = first
        .signal
        .send_signal(SignalInfo::new_kernel(Signo::SIGURG));
    assert_eq!(count.0.load(Ordering::Relaxed), 0);
    assert!(first.poll().contains(IoEvents::IN));
    assert!(second.poll().is_empty());
    let _ = second
        .signal
        .send_signal(SignalInfo::new_kernel(Signo::SIGURG));
    assert!(count.0.load(Ordering::Relaxed) > 0);
    assert!(second.poll().contains(IoEvents::IN));
}

#[def_test]
fn signalfd_mask_update_notifies_only_updaters_shared_source() {
    let process = Arc::new(ProcessSignalManager::new(SignalActions::default(), 0));
    let shared = Arc::new(process.fork_shared().unwrap());
    let private = Arc::new(process.fork_private().unwrap());
    let signalfd = Signalfd::new(SignalSet::default());
    let first = SignalfdAccess {
        signalfd: signalfd.clone(),
        signal: ThreadSignalManager::new(1, process),
    };
    let peer = SignalfdAccess {
        signalfd: signalfd.clone(),
        signal: ThreadSignalManager::new(2, shared),
    };
    let independent = SignalfdAccess {
        signalfd: signalfd.clone(),
        signal: ThreadSignalManager::new(3, private),
    };
    let mut mask = SignalSet::default();
    mask.add(Signo::SIGURG);
    peer.signal.set_blocked(mask);
    let _ = peer
        .signal
        .send_signal(SignalInfo::new_kernel(Signo::SIGURG));

    let shared_count = Arc::new(WakeCount(AtomicUsize::new(0)));
    let private_count = Arc::new(WakeCount(AtomicUsize::new(0)));
    let shared_waker = Waker::from(shared_count.clone());
    let private_waker = Waker::from(private_count.clone());
    let shared_cx = Context::from_waker(&shared_waker);
    let private_cx = Context::from_waker(&private_waker);
    let mut shared_registrations = PollRegistrations::new();
    let mut private_registrations = PollRegistrations::new();
    first
        .register(&mut shared_registrations.context(&shared_cx), IoEvents::IN)
        .unwrap();
    independent
        .register(
            &mut private_registrations.context(&private_cx),
            IoEvents::IN,
        )
        .unwrap();
    assert_eq!(shared_registrations.len(), 1);
    assert_eq!(private_registrations.len(), 1);
    signalfd.update_mask(mask, &peer.signal).unwrap();
    assert_eq!(shared_count.0.load(Ordering::Relaxed), 1);
    assert_eq!(private_count.0.load(Ordering::Relaxed), 0);
    assert_eq!(
        posix_types::k_sigset::from(peer.signal.blocked()).0,
        posix_types::k_sigset::from(mask).0
    );
    assert!(peer.poll().contains(IoEvents::IN));
    assert!(first.poll().is_empty());
    assert!(independent.poll().is_empty());
    assert_eq!(
        posix_types::k_sigset::from(independent.signalfd.mask()).0,
        posix_types::k_sigset::from(mask).0
    );
    assert_eq!(peer.dequeue_signal().unwrap().signo(), Signo::SIGURG);
}

#[def_test]
fn signalfd_child_exit_wakes_without_unblocked_delivery_target() {
    let process = Arc::new(ProcessSignalManager::new(SignalActions::default(), 0));
    let signal = ThreadSignalManager::new(1, process.clone());
    let mut mask = SignalSet::default();
    mask.add(Signo::SIGCHLD);
    signal.set_blocked(mask);
    let access = SignalfdAccess {
        signalfd: Signalfd::new(mask),
        signal,
    };
    let count = Arc::new(WakeCount(AtomicUsize::new(0)));
    let waker = Waker::from(count.clone());
    let cx = Context::from_waker(&waker);
    let mut registrations = PollRegistrations::new();
    access
        .register(&mut registrations.context(&cx), IoEvents::IN)
        .unwrap();
    let child = ChildExitInfo::from_wait_status(
        42,
        1000,
        0,
        ktime_types::TimeSpan::from_nanos(0),
        ktime_types::TimeSpan::from_nanos(0),
    );
    let (_, target) = process
        .send_child_exit_signal(ChildExitSignalInfo::new_sigchld(child))
        .unwrap();
    assert_eq!(target, None);
    assert!(count.0.load(Ordering::Relaxed) > 0);
    assert!(access.poll().contains(IoEvents::IN));
    assert_eq!(access.dequeue_signal().unwrap().signo(), Signo::SIGCHLD);
}

#[def_test]
fn test_signalfd_poll_empty() {
    let file = Signalfd::new_file(SignalSet::default(), 0, kcred::initial_cred())
        .expect("signalfd file opens");
    assert!(file.poll().contains(IoEvents::ERR));
    assert!(matches!(
        file.capture_poll_source(),
        Err(PollRegisterError::InvalidState)
    ));
}

#[def_test]
fn test_signalfd_siginfo_size() {
    assert_eq!(SIGNALFD_SIGINFO_SIZE, 128);
    assert_eq!(core::mem::size_of::<SignalfdSiginfo>(), 128);
}

#[def_test]
fn test_signalfd_nonblocking() {
    let file = Signalfd::new_file(SignalSet::default(), 0, kcred::initial_cred())
        .expect("signalfd file opens");
    assert!(!file.is_nonblocking());
    file.set_nonblocking(true);
    assert!(file.is_nonblocking());
    file.set_nonblocking(false);
    assert!(!file.is_nonblocking());
}

#[def_test]
fn test_signalfd_write_returns_error() {
    let file = Signalfd::new_file(SignalSet::default(), 0, kcred::initial_cred())
        .expect("signalfd file opens");
    let mut pos = 0;
    assert!(file.write_from(b"test", &mut pos).is_err());
}

#[def_test]
fn test_signalfd_siginfo_from_signal_info() {
    let sig = SignalInfo::new_kernel(Signo::SIGUSR1);
    let info = SignalfdSiginfo::from_signal_info(&sig);
    assert_eq!(info.ssi_signo, Signo::SIGUSR1 as u32);
    assert_eq!(info.ssi_fd, -1);
    assert_eq!(info.ssi_pid, 0);
}

#[def_test]
fn test_signalfd_siginfo_from_child_exit_signal_info() {
    let child = ChildExitInfo::from_wait_status(
        42,
        1000,
        9 << 8,
        ktime_types::TimeSpan::from_millis(20),
        ktime_types::TimeSpan::from_millis(30),
    );
    let sig = ChildExitSignalInfo::new_sigchld(child);
    let info = SignalfdSiginfo::from_signal_info(sig.as_child_exit_signal().as_signal_info());

    assert_eq!(info.ssi_signo, Signo::SIGCHLD as u32);
    assert_eq!(info.ssi_code, linux_raw_sys::general::CLD_EXITED as i32);
    assert_eq!(info.ssi_pid, 42);
    assert_eq!(info.ssi_uid, 1000);
    assert_eq!(info.ssi_status, 9);
    assert_eq!(
        info.ssi_utime,
        posix_types::PosixClockTicks::from_time_span(ktime_types::TimeSpan::from_millis(20))
            .as_raw()
    );
    assert_eq!(
        info.ssi_stime,
        posix_types::PosixClockTicks::from_time_span(ktime_types::TimeSpan::from_millis(30))
            .as_raw()
    );
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Live ownership tests; boot services remain installed until guest exit.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use ktime_types::TimeSpan;
use unittest::def_test;

struct Capture {
    drops: Arc<AtomicUsize>,
    running: Arc<AtomicBool>,
}

impl Capture {
    fn run(&self) {
        assert_eq!(self.drops.load(Ordering::Acquire), 0);
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        assert!(ktask::is_sleepable_context());
        assert!(!self.running.load(Ordering::Acquire));
        self.drops.fetch_add(1, Ordering::Release);
    }
}

fn wait_until(mut done: impl FnMut() -> bool) {
    let limit = khal::time::monotonic_time() + TimeSpan::from_secs(5);
    while !done() && khal::time::monotonic_time() < limit {
        ktask::sleep(TimeSpan::from_millis(1));
    }
    assert!(done(), "owned work did not complete reclamation");
}

/// Last-handle release in IRQ-masked context safely retires idle/pending/running Work.
///
/// # Panics
/// Panics on failed admission, premature/duplicate destruction, wrong destructor
/// context, post-shutdown admission or missing bounded reclamation progress.
#[def_test(serial)]
fn owned_work_drains_and_destroys_captures_in_task_context() {
    for mode in 0..3 {
        let drops = Arc::new(AtomicUsize::new(0));
        let running = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let capture = Capture {
            drops: drops.clone(),
            running: running.clone(),
        };
        let callback_running = running.clone();
        let callback_release = release.clone();
        let handle = crate::WorkHandle::new(move || {
            capture.run();
            callback_running.store(true, Ordering::Release);
            if mode == 2 {
                while !callback_release.load(Ordering::Acquire) {
                    ktask::sleep(TimeSpan::from_millis(1));
                }
            }
            callback_running.store(false, Ordering::Release);
        })
        .unwrap();
        if mode == 2 {
            handle.queue_default(None).unwrap();
            wait_until(|| running.load(Ordering::Acquire));
        }
        {
            let _irq = kspin::NoPreemptIrqSave::new();
            if mode == 1 {
                handle.queue_default(None).unwrap();
            }
            drop(handle);
            assert_eq!(drops.load(Ordering::Acquire), 0);
        }
        release.store(true, Ordering::Release);
        wait_until(|| drops.load(Ordering::Acquire) == 1 && Arc::strong_count(&drops) == 1);
    }

    let handle = crate::WorkHandle::new(|| {}).unwrap();
    let clone = handle.clone();
    handle.queue_default(None).unwrap();
    handle.flush().unwrap();
    handle.shutdown().unwrap();
    assert_eq!(clone.queue_default(None), Err(crate::SubmitError::Closed));
    clone.shutdown().unwrap();
}

/// A callback can release its last public owner without waiting on itself.
///
/// # Panics
/// Panics on failed submission, wrong destruction context or missing reclamation.
#[def_test(serial)]
fn owned_callback_can_release_last_handle() {
    type Handle = crate::WorkHandle<alloc::boxed::Box<dyn Fn() + Send + Sync>>;
    let slot: Arc<kspin::SpinNoIrq<Option<Handle>>> = Arc::new(kspin::SpinNoIrq::new(None));
    let drops = Arc::new(AtomicUsize::new(0));
    let running = Arc::new(AtomicBool::new(false));
    let capture = Capture {
        drops: drops.clone(),
        running: running.clone(),
    };
    let callback_slot = slot.clone();
    let callback: alloc::boxed::Box<dyn Fn() + Send + Sync> = alloc::boxed::Box::new(move || {
        capture.run();
        running.store(true, Ordering::Release);
        let handle = callback_slot.lock().take().unwrap();
        drop(handle);
        running.store(false, Ordering::Release);
    });
    *slot.lock() = Some(crate::WorkHandle::new(callback).unwrap());
    slot.lock().as_ref().unwrap().queue_default(None).unwrap();
    wait_until(|| drops.load(Ordering::Acquire) == 1);
    assert!(slot.lock().is_none());
}

/// Shutdown excludes racing producers across all clones without closing on IRQ error.
///
/// # Panics
/// Panics on unexpected submission results, failed join or post-close execution.
#[def_test(serial)]
fn owned_shutdown_excludes_racing_publication() {
    let calls = Arc::new(AtomicUsize::new(0));
    let callback_calls = calls.clone();
    let handle = crate::WorkHandle::new(move || {
        callback_calls.fetch_add(1, Ordering::Relaxed);
    })
    .unwrap();
    {
        let _irq = kspin::NoPreemptIrqSave::new();
        assert_eq!(handle.shutdown(), Err(crate::WaitError::InvalidContext));
        handle.queue_default(None).unwrap();
        assert!(handle.cancel_pending());
    }
    let started = Arc::new(AtomicBool::new(false));
    let producer_started = started.clone();
    let producer_handle = handle.clone();
    let producer = ktask::spawn(move || {
        loop {
            match producer_handle.queue_default(None) {
                Ok(_) | Err(crate::SubmitError::AlreadyQueued | crate::SubmitError::Disabled) => {}
                Err(crate::SubmitError::Closed) => break,
                result => panic!("unexpected owned admission: {result:?}"),
            }
            producer_started.store(true, Ordering::Release);
            ktask::yield_now();
        }
    });
    wait_until(|| started.load(Ordering::Acquire));
    handle.shutdown().unwrap();
    let completed = calls.load(Ordering::Acquire);
    assert_eq!(producer.join(), 0);
    assert_eq!(handle.queue_default(None), Err(crate::SubmitError::Closed));
    assert_eq!(calls.load(Ordering::Acquire), completed);
}

/// Owned delayed work expedites, cancels, retimes and permanently closes all clones.
///
/// # Panics
/// Panics on lost/duplicate execution, wrong closure or admission/drain failures.
#[def_test(serial)]
fn owned_delayed_reuse_and_shutdown() {
    let calls = Arc::new(AtomicUsize::new(0));
    let callback_calls = calls.clone();
    let handle = crate::DelayedWorkHandle::new(move || {
        assert!(ktask::is_sleepable_context());
        callback_calls.fetch_add(1, Ordering::Release);
    })
    .unwrap();
    let later = TimeSpan::from_secs(60);
    handle.queue_default(later, None).unwrap();
    handle.flush().unwrap();
    assert_eq!(calls.load(Ordering::Acquire), 1);
    for _ in 0..16 {
        let _irq = kspin::NoPreemptIrqSave::new();
        handle.queue_default(later, None).unwrap();
        handle.modify_default(TimeSpan::ZERO, None).unwrap();
        handle.modify_default(later, None).unwrap();
        assert!(handle.cancel_pending());
        assert_eq!(handle.shutdown(), Err(crate::WaitError::InvalidContext));
    }
    handle
        .queue_default(TimeSpan::from_millis(1), None)
        .unwrap();
    wait_until(|| calls.load(Ordering::Acquire) == 2);
    handle.cancel_sync().unwrap();
    let clone = handle.clone();
    handle.queue_default(later, None).unwrap();
    handle.shutdown().unwrap();
    assert_eq!(
        clone.modify_default(TimeSpan::ZERO, None),
        Err(crate::ArmError::Admission(crate::SubmitError::Closed))
    );
    assert_eq!(
        clone.queue_default(later, None),
        Err(crate::ArmError::Admission(crate::SubmitError::Closed))
    );
    assert_eq!(calls.load(Ordering::Acquire), 2);
}

/// Delayed final release retains captures through pending/racing/running timer work.
///
/// # Panics
/// Panics on early destruction, wrong destructor context or incomplete reclamation.
#[def_test(serial)]
fn owned_delayed_final_release_drains_timer_and_callback() {
    for mode in 0..4 {
        for _ in 0..16 {
            let drops = Arc::new(AtomicUsize::new(0));
            let running = Arc::new(AtomicBool::new(false));
            let release = Arc::new(AtomicBool::new(false));
            let capture = Capture {
                drops: drops.clone(),
                running: running.clone(),
            };
            let callback_running = running.clone();
            let callback_release = release.clone();
            let handle = crate::DelayedWorkHandle::new(move || {
                capture.run();
                callback_running.store(true, Ordering::Release);
                if mode == 3 {
                    while !callback_release.load(Ordering::Acquire) {
                        ktask::sleep(TimeSpan::from_millis(1));
                    }
                }
                callback_running.store(false, Ordering::Release);
            })
            .unwrap();
            match mode {
                0 => {}
                1 => {
                    handle.queue_default(TimeSpan::from_secs(60), None).unwrap();
                }
                2 => {
                    handle.queue_default(TimeSpan::from_nanos(1), None).unwrap();
                }
                _ => {
                    handle.queue_default(TimeSpan::ZERO, None).unwrap();
                    wait_until(|| running.load(Ordering::Acquire));
                    handle.queue_default(TimeSpan::from_secs(60), None).unwrap();
                }
            }
            {
                let _irq = kspin::NoPreemptIrqSave::new();
                drop(handle);
            }
            release.store(true, Ordering::Release);
            wait_until(|| drops.load(Ordering::Acquire) == 1 && Arc::strong_count(&drops) == 1);
        }
    }
}

/// Concurrent retiming cannot rearm an owned timer after shutdown returns.
///
/// # Panics
/// Panics on wrong modification results, post-close execution or failed join/drain.
#[def_test(serial)]
fn owned_delayed_shutdown_excludes_retiming() {
    let calls = Arc::new(AtomicUsize::new(0));
    let callback_calls = calls.clone();
    let handle = crate::DelayedWorkHandle::new(move || {
        callback_calls.fetch_add(1, Ordering::Release);
    })
    .unwrap();
    let started = Arc::new(AtomicBool::new(false));
    let producer_started = started.clone();
    let producer_handle = handle.clone();
    let producer = ktask::spawn(move || {
        let mut delay = TimeSpan::ZERO;
        loop {
            match producer_handle.modify_default(delay, None) {
                Ok(_) | Err(crate::ArmError::Admission(crate::SubmitError::Disabled)) => {}
                Err(crate::ArmError::Admission(crate::SubmitError::Closed)) => break,
                result => panic!("unexpected owned retime: {result:?}"),
            }
            producer_started.store(true, Ordering::Release);
            delay = if delay.is_zero() {
                TimeSpan::from_millis(1)
            } else {
                TimeSpan::ZERO
            };
            ktask::yield_now();
        }
    });
    wait_until(|| started.load(Ordering::Acquire));
    handle.shutdown().unwrap();
    let completed = calls.load(Ordering::Acquire);
    assert_eq!(producer.join(), 0);
    ktask::sleep(TimeSpan::from_millis(2));
    assert_eq!(calls.load(Ordering::Acquire), completed);
}

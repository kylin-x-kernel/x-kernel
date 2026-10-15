// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::{
    prelude::v1::*,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32},
        mpsc::channel,
    },
    thread,
};

use super::*;

#[test]
fn smoke_once() {
    static O: Once = Once::new();
    let mut a = 0;
    O.call_once(|| a += 1);
    assert_eq!(a, 1);
    O.call_once(|| a += 1);
    assert_eq!(a, 1);
}

#[test]
fn smoke_once_value() {
    static O: Once<usize> = Once::new();
    let a = O.call_once(|| 1);
    assert_eq!(*a, 1);
    let b = O.call_once(|| 2);
    assert_eq!(*b, 1);
}

#[test]
fn stampede_once() {
    static O: Once = Once::new();
    static RUN: AtomicBool = AtomicBool::new(false);

    let (tx, rx) = channel();
    let mut ts = Vec::new();
    for _ in 0..10 {
        let tx = tx.clone();
        ts.push(thread::spawn(move || {
            for _ in 0..4 {
                thread::yield_now()
            }
            O.call_once(|| {
                assert!(!RUN.load(Ordering::Acquire));
                RUN.store(true, Ordering::Release);
            });
            assert!(RUN.load(Ordering::Acquire));
            tx.send(()).unwrap();
        }));
    }

    O.call_once(|| {
        assert!(!RUN.load(Ordering::Acquire));
        RUN.store(true, Ordering::Release);
    });
    assert!(RUN.load(Ordering::Acquire));

    for _ in 0..10 {
        rx.recv().unwrap();
    }

    for t in ts {
        t.join().unwrap();
    }
}

#[test]
fn get() {
    static INIT: Once<usize> = Once::new();

    assert!(INIT.get().is_none());
    INIT.call_once(|| 2);
    assert_eq!(INIT.get().map(|r| *r), Some(2));
}

#[test]
fn get_no_wait() {
    static INIT: Once<usize> = Once::new();

    assert!(INIT.get().is_none());
    let t = thread::spawn(move || {
        INIT.call_once(|| {
            thread::sleep(std::time::Duration::from_secs(3));
            42
        });
    });
    assert!(INIT.get().is_none());

    t.join().unwrap();
}

#[test]
fn poll() {
    static INIT: Once<usize> = Once::new();

    assert!(INIT.poll().is_none());
    INIT.call_once(|| 3);
    assert_eq!(INIT.poll().map(|r| *r), Some(3));
}

#[test]
fn wait() {
    static INIT: Once<usize> = Once::new();

    let t = std::thread::spawn(|| {
        assert_eq!(*INIT.wait(), 3);
        assert!(INIT.is_completed());
    });

    for _ in 0..4 {
        thread::yield_now()
    }

    assert!(INIT.poll().is_none());
    INIT.call_once(|| 3);

    t.join().unwrap();
}

#[test]
fn panic() {
    use std::panic;

    static INIT: Once = Once::new();

    // poison the once
    let t = panic::catch_unwind(|| {
        INIT.call_once(|| panic!());
    });
    assert!(t.is_err());

    // poisoning propagates
    let t = panic::catch_unwind(|| {
        INIT.call_once(|| {});
    });
    assert!(t.is_err());
}

#[test]
fn init_constant() {
    static O: Once = Once::INIT;
    let mut a = 0;
    O.call_once(|| a += 1);
    assert_eq!(a, 1);
    O.call_once(|| a += 1);
    assert_eq!(a, 1);
}

static CALLED: AtomicBool = AtomicBool::new(false);

struct DropTest {}

impl Drop for DropTest {
    fn drop(&mut self) {
        CALLED.store(true, Ordering::Release);
    }
}

#[test]
fn try_call_once_err() {
    let once = Once::<_>::new();
    let shared = Arc::new((once, AtomicU32::new(0)));

    let (tx, rx) = channel();

    let t0 = {
        let shared = shared.clone();
        thread::spawn(move || {
            let (once, called) = &*shared;

            let result = once.try_call_once(|| {
                called.fetch_add(1, Ordering::AcqRel);
                tx.send(()).unwrap();
                thread::sleep(std::time::Duration::from_millis(50));
                Err(())
            });
            assert_eq!(result, Err(()));
        })
    };

    let t1 = {
        let shared = shared.clone();
        thread::spawn(move || {
            rx.recv().unwrap();
            let (once, called) = &*shared;
            assert_eq!(
                called.load(Ordering::Acquire),
                1,
                "leader thread did not run first"
            );

            once.call_once(|| {
                called.fetch_add(1, Ordering::AcqRel);
            });
        })
    };

    t0.join().unwrap();
    t1.join().unwrap();

    assert_eq!(shared.1.load(Ordering::Acquire), 2);
}

// This is sort of two test cases, but if we write them as separate test methods
// they can be executed concurrently and then fail some small fraction of the
// time.
#[test]
fn drop_occurs_and_skip_uninit_drop() {
    CALLED.store(false, Ordering::Release);

    {
        let once = Once::<_>::new();
        once.call_once(|| DropTest {});
    }

    assert!(CALLED.load(Ordering::Acquire));
    // Now test that we skip drops for the uninitialized case.
    CALLED.store(false, Ordering::Release);

    let once = Once::<DropTest>::new();
    drop(once);

    assert!(!CALLED.load(Ordering::Acquire));
}

#[test]
fn call_once_test() {
    for _ in 0..20 {
        use std::{
            sync::{Arc, atomic::AtomicUsize},
            time::Duration,
        };
        let share = Arc::new(AtomicUsize::new(0));
        let once = Arc::new(Once::<_>::new());
        let mut hs = Vec::new();
        for _ in 0..8 {
            let h = thread::spawn({
                let share = share.clone();
                let once = once.clone();
                move || {
                    thread::sleep(Duration::from_millis(10));
                    once.call_once(|| {
                        share.fetch_add(1, Ordering::SeqCst);
                    });
                }
            });
            hs.push(h);
        }
        for h in hs {
            h.join().unwrap();
        }
        assert_eq!(1, share.load(Ordering::SeqCst));
    }
}

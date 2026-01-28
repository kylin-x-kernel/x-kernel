use std::sync::Once;

use ksync::Mutex;
use ktask as thread;

static INIT: Once = Once::new();

fn may_interrupt() {
    // simulate interrupts
    if fastrand::u8(0..3) == 0 {
        thread::yield_now();
    }
}

#[test]
fn mutex_basic() {
    INIT.call_once(thread::init_scheduler);

    let m = Mutex::new(0);
    *m.lock() = 42;
    assert_eq!(*m.lock(), 42);
}

#[test]
fn mutex_concurrent() {
    INIT.call_once(thread::init_scheduler);

    const NUM_TASKS: u32 = 10;
    const NUM_ITERS: u32 = 1000;
    static M: Mutex<u32> = Mutex::new(0);

    fn inc(delta: u32) {
        for _ in 0..NUM_ITERS {
            let mut val = M.lock();
            *val += delta;
            may_interrupt();
            drop(val);
            may_interrupt();
        }
    }

    for _ in 0..NUM_TASKS {
        thread::spawn(|| inc(1));
        thread::spawn(|| inc(2));
    }

    loop {
        let val = M.lock();
        if *val == NUM_ITERS * NUM_TASKS * 3 {
            break;
        }
        may_interrupt();
        drop(val);
        may_interrupt();
    }

    assert_eq!(*M.lock(), NUM_ITERS * NUM_TASKS * 3);
}

#[test]
#[cfg(feature = "stats")]
fn mutex_stats() {
    INIT.call_once(thread::init_scheduler);

    let m = Mutex::new(0);
    m.reset_stats();

    {
        let _g = m.lock();
        let (locks, ..) = m.stats();
        assert_eq!(locks, 1);
    }

    {
        let _g = m.lock();
        let (locks, ..) = m.stats();
        assert_eq!(locks, 2);
    }
}

#[test]
fn mutex_try_lock() {
    INIT.call_once(thread::init_scheduler);

    let m = Mutex::new(0);

    let g1 = m.try_lock();
    assert!(g1.is_some());

    let g2 = m.try_lock();
    assert!(g2.is_none());

    drop(g1);

    let g3 = m.try_lock();
    assert!(g3.is_some());
}

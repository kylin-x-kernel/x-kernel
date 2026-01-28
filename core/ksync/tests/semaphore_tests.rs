use std::sync::{
    Arc, Once,
    atomic::{AtomicU32, Ordering},
};

use ksync::Semaphore;
use ktask as thread;

static INIT: Once = Once::new();

#[test]
fn semaphore_basic() {
    INIT.call_once(thread::init_scheduler);

    let sem = Semaphore::new(3);

    assert_eq!(sem.available_permits(), 3);

    let _g1 = sem.acquire_guard();
    assert_eq!(sem.available_permits(), 2);

    let _g2 = sem.acquire_guard();
    assert_eq!(sem.available_permits(), 1);

    let _g3 = sem.acquire_guard();
    assert_eq!(sem.available_permits(), 0);

    // All permits used
    assert!(!sem.try_acquire());

    drop(_g1);
    assert_eq!(sem.available_permits(), 1);

    // One permit released
    assert!(sem.try_acquire());
}

#[test]
fn semaphore_acquire_release() {
    INIT.call_once(thread::init_scheduler);

    let sem = Semaphore::new(2);

    sem.acquire();
    assert_eq!(sem.available_permits(), 1);

    sem.acquire();
    assert_eq!(sem.available_permits(), 0);

    sem.release();
    assert_eq!(sem.available_permits(), 1);

    sem.release();
    assert_eq!(sem.available_permits(), 2);
}

#[test]
fn semaphore_concurrent() {
    INIT.call_once(thread::init_scheduler);

    static COUNTER: AtomicU32 = AtomicU32::new(0);
    static MAX_COUNTER: AtomicU32 = AtomicU32::new(0);
    let sem = Arc::new(Semaphore::new(3));
    let mut handles = vec![];

    for _ in 0..10 {
        let sem = sem.clone();
        let handle = thread::spawn(move || {
            let _g = sem.acquire_guard();

            let count = COUNTER.fetch_add(1, Ordering::SeqCst) + 1;

            // Update max counter
            loop {
                let max = MAX_COUNTER.load(Ordering::SeqCst);
                if count <= max {
                    break;
                }
                if MAX_COUNTER
                    .compare_exchange(max, count, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    break;
                }
            }

            // Verify at most 3 concurrent accesses
            assert!(count <= 3, "too many concurrent accesses: {}", count);

            thread::yield_now();

            COUNTER.fetch_sub(1, Ordering::SeqCst);
        });
        handles.push(handle);
    }

    for h in handles {
        h.join();
    }

    // Verify that we had at most 3 concurrent accesses
    assert!(MAX_COUNTER.load(Ordering::SeqCst) <= 3);
    assert_eq!(COUNTER.load(Ordering::SeqCst), 0);
}

#[test]
fn semaphore_try_acquire() {
    INIT.call_once(thread::init_scheduler);

    let sem = Semaphore::new(1);

    assert!(sem.try_acquire());
    assert_eq!(sem.available_permits(), 0);

    assert!(!sem.try_acquire());

    sem.release();
    assert_eq!(sem.available_permits(), 1);

    assert!(sem.try_acquire());
}

#[test]
fn semaphore_guard_drop() {
    INIT.call_once(thread::init_scheduler);

    let sem = Semaphore::new(1);

    {
        let _g = sem.acquire_guard();
        assert_eq!(sem.available_permits(), 0);
    }

    // Guard dropped, permit should be released
    assert_eq!(sem.available_permits(), 1);
}

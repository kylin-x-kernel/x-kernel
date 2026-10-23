// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
};

use crate::{
    allocation::measure,
    completion::{Completion, SyncWake, WaitQueue},
};

#[derive(Default)]
struct Counter(AtomicUsize);

// SAFETY: Only an atomic increment; no allocation, locks, unwind, retention,
// or reentry into completion code.
unsafe impl SyncWake for Counter {
    fn notify(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

struct Parker {
    thread: thread::Thread,
    notified: AtomicBool,
}

impl Parker {
    fn current() -> Self {
        let thread = thread::current();
        thread.unpark();
        thread::park();
        Self {
            thread,
            notified: AtomicBool::new(false),
        }
    }

    fn park(&self) {
        while !self.notified.swap(false, Ordering::AcqRel) {
            thread::park();
        }
    }
}

// SAFETY: Atomic latch plus an already initialized Thread::unpark; no borrowed
// storage escapes, no completion reentry or user callbacks. This is the host
// notification substitute, not a claim about kernel IRQ context.
unsafe impl SyncWake for Parker {
    fn notify(&self) {
        self.notified.store(true, Ordering::Release);
        self.thread.unpark();
    }
}

#[test]
fn allocation_counter_detects_allocations_and_frees() {
    let counts = measure(|| {
        let value = Box::new(42usize);
        std::hint::black_box(&value);
        drop(value);
    });
    assert_eq!(counts, (1, 1));
}

#[test]
fn construction_and_already_ready_paths_allocate_and_free_nothing() {
    assert_eq!(
        measure(|| {
            let completion = Completion::new();
            let queue = WaitQueue::new();
            let wake = Counter::default();
            assert_eq!(completion.complete(), 0);
            {
                let wait_wake = &wake;
                let wait_park = || panic!("ready completion parked");
                // SAFETY: These host closures return or unwind normally; no nonunwinding
                // thread exit can reclaim the stack, source or borrowed wake before cleanup.
                unsafe { completion.wait_with(wait_wake, wait_park) }
            };
            assert!(!completion.try_wait());
            {
                let wait_wake = &wake;
                let wait_ready = || true;
                let wait_park = || panic!("ready predicate parked");
                // SAFETY: These host closures return or unwind normally; no nonunwinding
                // thread exit can reclaim the stack, source or borrowed wake before cleanup.
                unsafe { queue.wait_until(wait_wake, wait_ready, wait_park) }
            };
            assert_eq!(queue.notify_all(), 0);
        }),
        (0, 0)
    );
}

#[test]
fn repeated_spurious_waits_do_not_allocate_or_duplicate_nodes() {
    let completion = Completion::new();
    let wake = Counter::default();
    assert_eq!(
        measure(|| {
            for _ in 0..128 {
                let mut parks = 0;
                {
                    let wait_wake = &wake;
                    let wait_park = || {
                        parks += 1;
                        if parks == 4 {
                            assert_eq!(completion.complete(), 1);
                        }
                    };
                    // SAFETY: These host closures return or unwind normally; no nonunwinding
                    // thread exit can reclaim the stack, source or borrowed wake before cleanup.
                    unsafe { completion.wait_with(wait_wake, wait_park) }
                };
                assert_eq!(parks, 4);
                assert!(!completion.is_completed());
            }
        }),
        (0, 0)
    );
    assert_eq!(wake.0.load(Ordering::Relaxed), 128);
    assert_eq!(completion.complete_all(), 0);
}

#[test]
fn fifo_notification_token_stealing_and_cleanup() {
    let completion = Completion::new();
    let first = Counter::default();
    let second = Counter::default();
    let mut parks = 0;
    assert_eq!(
        measure(|| {
            let wait_wake = &first;
            let wait_park = || {
                parks += 1;
                if parks == 1 {
                    {
                        let wait_wake = &second;
                        let wait_park = || {
                            assert_eq!(completion.complete(), 1);
                            assert_eq!(first.0.load(Ordering::Relaxed), 1);
                            assert_eq!(second.0.load(Ordering::Relaxed), 0);
                        };
                        // SAFETY: These host closures return or unwind normally; no nonunwinding
                        // thread exit can reclaim the stack, source or borrowed wake before cleanup.
                        unsafe { completion.wait_with(wait_wake, wait_park) }
                    };
                } else {
                    assert_eq!(completion.complete(), 1);
                }
            };
            // SAFETY: These host closures return or unwind normally; no nonunwinding
            // thread exit can reclaim the stack, source or borrowed wake before cleanup.
            unsafe { completion.wait_with(wait_wake, wait_park) }
        }),
        (0, 0)
    );
    assert_eq!(parks, 2);
    assert_eq!(completion.complete_all(), 0);
}

#[test]
fn broadcast_wakes_multiple_distinct_wake_types() {
    let completion = Completion::new();
    let first = Counter::default();
    let second = Parker::current();
    assert_eq!(
        measure(|| {
            let wait_wake = &first;
            let wait_park = || {
                {
                    let wait_wake = &second;
                    let wait_park = || assert_eq!(completion.complete_all(), 2);
                    // SAFETY: These host closures return or unwind normally; no nonunwinding
                    // thread exit can reclaim the stack, source or borrowed wake before cleanup.
                    unsafe { completion.wait_with(wait_wake, wait_park) }
                };
            };
            // SAFETY: These host closures return or unwind normally; no nonunwinding
            // thread exit can reclaim the stack, source or borrowed wake before cleanup.
            unsafe { completion.wait_with(wait_wake, wait_park) }
        }),
        (0, 0)
    );
    assert_eq!(first.0.load(Ordering::Relaxed), 1);
    assert!(second.notified.load(Ordering::Acquire));
    assert!(completion.try_wait());
    assert!(completion.try_wait());
    completion.reinit();
    assert!(!completion.try_wait());
}

#[test]
fn predicate_reentry_and_repeated_spurious_wakes_allocate_nothing() {
    let queue = WaitQueue::new();
    let wake = Counter::default();
    assert_eq!(
        measure(|| {
            for _ in 0..128 {
                let mut checks = 0;
                let mut parks = 0;
                {
                    let wait_wake = &wake;
                    let wait_ready = || {
                        checks += 1;
                        if checks % 2 == 0 {
                            assert_eq!(queue.notify_all(), 1);
                        }
                        checks == 4
                    };
                    let wait_park = || parks += 1;
                    // SAFETY: These host closures return or unwind normally; no nonunwinding
                    // thread exit can reclaim the stack, source or borrowed wake before cleanup.
                    unsafe { queue.wait_until(wait_wake, wait_ready, wait_park) }
                };
                assert_eq!(parks, 3);
                assert_eq!(queue.notify_all(), 0);
            }
        }),
        (0, 0)
    );
}

#[test]
fn predicate_broadcast_notifies_both_readers() {
    let queue = WaitQueue::new();
    let first = Counter::default();
    let second = Counter::default();
    let ready = AtomicBool::new(false);
    assert_eq!(
        measure(|| {
            let wait_wake = &first;
            let wait_ready = || ready.load(Ordering::Acquire);
            let wait_park = || {
                let wait_wake = &second;
                let wait_ready = || ready.load(Ordering::Acquire);
                let wait_park = || {
                    ready.store(true, Ordering::Release);
                    assert_eq!(queue.notify_all(), 2);
                };
                // SAFETY: These host closures return or unwind normally; no nonunwinding
                // thread exit can reclaim the stack, source or borrowed wake before cleanup.
                unsafe { queue.wait_until(wait_wake, wait_ready, wait_park) }
            };
            // SAFETY: These host closures return or unwind normally; no nonunwinding
            // thread exit can reclaim the stack, source or borrowed wake before cleanup.
            unsafe { queue.wait_until(wait_wake, wait_ready, wait_park) }
        }),
        (0, 0)
    );
    assert_eq!(first.0.load(Ordering::Relaxed), 1);
    assert_eq!(second.0.load(Ordering::Relaxed), 1);
}

#[test]
fn deferred_publication_does_not_notify_and_retains_owner() {
    let completion = Arc::new(Completion::new());
    let weak = Arc::downgrade(&completion);
    let wake = Counter::default();
    let mut deferred = None;
    assert_eq!(
        measure(|| {
            let wait_wake = &wake;
            let wait_park = || {
                deferred = Some(completion.complete_all_defer_wake());
                assert_eq!(wake.0.load(Ordering::Relaxed), 0);
            };
            // SAFETY: These host closures return or unwind normally; no nonunwinding
            // thread exit can reclaim the stack, source or borrowed wake before cleanup.
            unsafe { completion.wait_with(wait_wake, wait_park) }
        }),
        (0, 0)
    );
    drop(completion);
    assert!(weak.upgrade().is_some());
    assert_eq!(deferred.take().unwrap().wake(), 0);
    assert!(weak.upgrade().is_none());
}

#[test]
fn deferred_dispatch_with_live_owner_allocates_and_frees_nothing() {
    let completion = Arc::new(Completion::new());
    let wake = Counter::default();
    assert_eq!(
        measure(|| {
            let wait_wake = &wake;
            let wait_park = || {
                let deferred = completion.complete_all_defer_wake();
                assert_eq!(wake.0.load(Ordering::Relaxed), 0);
                assert_eq!(deferred.wake(), 1);
            };
            // SAFETY: These host closures return or unwind normally; no nonunwinding
            // thread exit can reclaim the stack, source or borrowed wake before cleanup.
            unsafe { completion.wait_with(wait_wake, wait_park) }
        }),
        (0, 0)
    );
}

#[test]
fn delayed_dispatch_after_reset_only_prompts_recheck() {
    let completion = Arc::new(Completion::new());
    let deferred = completion.complete_all_defer_wake();
    completion.reinit();
    let wake = Counter::default();
    let mut deferred = Some(deferred);
    let mut parks = 0;
    assert_eq!(
        measure(|| {
            let wait_wake = &wake;
            let wait_park = || {
                parks += 1;
                if let Some(delayed) = deferred.take() {
                    assert_eq!(delayed.wake(), 1);
                    assert!(!completion.is_completed());
                } else {
                    assert_eq!(completion.complete(), 1);
                }
            };
            // SAFETY: These host closures return or unwind normally; no nonunwinding
            // thread exit can reclaim the stack, source or borrowed wake before cleanup.
            unsafe { completion.wait_with(wait_wake, wait_park) }
        }),
        (0, 0)
    );
    assert_eq!(parks, 2);
    assert_eq!(wake.0.load(Ordering::Relaxed), 2);
}

#[test]
fn unwinding_closures_unlink_before_stack_reclamation() {
    let completion = Completion::new();
    let queue = WaitQueue::new();
    let wake = Counter::default();
    assert!(
        catch_unwind(AssertUnwindSafe(|| {
            {
                let wait_wake = &wake;
                let wait_park = || panic!("park unwind");
                // SAFETY: These host closures return or unwind normally; no nonunwinding
                // thread exit can reclaim the stack, source or borrowed wake before cleanup.
                unsafe { completion.wait_with(wait_wake, wait_park) }
            };
        }))
        .is_err()
    );
    assert!(
        catch_unwind(AssertUnwindSafe(|| {
            {
                let wait_wake = &wake;
                let wait_ready = || panic!("predicate unwind");
                let wait_park = || {};
                // SAFETY: These host closures return or unwind normally; no nonunwinding
                // thread exit can reclaim the stack, source or borrowed wake before cleanup.
                unsafe { queue.wait_until(wait_wake, wait_ready, wait_park) }
            };
        }))
        .is_err()
    );
    assert_eq!(completion.complete_all(), 0);
    assert_eq!(queue.notify_all(), 0);
    assert_eq!(wake.0.load(Ordering::Relaxed), 0);
}

#[test]
fn blocking_cross_thread_wait_and_notification_allocate_nothing() {
    let completion = Completion::new();
    let entered = AtomicBool::new(false);
    thread::scope(|scope| {
        let waiter = scope.spawn(|| {
            let wake = Parker::current();
            measure(|| {
                {
                    let wait_wake = &wake;
                    let wait_park = || {
                        entered.store(true, Ordering::Release);
                        wake.park();
                    };
                    // SAFETY: These host closures return or unwind normally; no nonunwinding
                    // thread exit can reclaim the stack, source or borrowed wake before cleanup.
                    unsafe { completion.wait_with(wait_wake, wait_park) }
                }
            })
        });
        while !entered.load(Ordering::Acquire) {
            thread::yield_now();
        }
        assert_eq!(measure(|| assert_eq!(completion.complete(), 1)), (0, 0));
        assert_eq!(waiter.join().unwrap(), (0, 0));
    });
    assert!(!completion.try_wait());
}

#[test]
fn predicate_cross_thread_publication_and_repeated_waits() {
    let queue = WaitQueue::new();
    let published = AtomicUsize::new(0);
    let observed = AtomicUsize::new(0);
    thread::scope(|scope| {
        let waiter = scope.spawn(|| {
            let wake = Parker::current();
            measure(|| {
                for round in 1..=256 {
                    {
                        let wait_wake = &wake;
                        let wait_ready = || published.load(Ordering::Acquire) >= round;
                        let wait_park = || wake.park();
                        // SAFETY: These host closures return or unwind normally; no nonunwinding
                        // thread exit can reclaim the stack, source or borrowed wake before cleanup.
                        unsafe { queue.wait_until(wait_wake, wait_ready, wait_park) }
                    };
                    observed.store(round, Ordering::Release);
                }
            })
        });
        let counts = measure(|| {
            for round in 1..=256 {
                published.store(round, Ordering::Release);
                queue.notify_all();
                while observed.load(Ordering::Acquire) < round {
                    thread::yield_now();
                }
            }
        });
        assert_eq!(counts, (0, 0));
        assert_eq!(waiter.join().unwrap(), (0, 0));
    });
    assert_eq!(queue.notify_all(), 0);
}

#[test]
fn unlink_head_middle_and_tail_preserves_other_waiters() {
    for removed in 0..3 {
        for unwind in [false, true] {
            let queue = WaitQueue::new();
            let entered = [const { AtomicBool::new(false) }; 3];
            let ready = [const { AtomicBool::new(false) }; 3];
            thread::scope(|scope| {
                let mut waiters = Vec::new();
                for index in 0..3 {
                    let queue = &queue;
                    let entered = &entered;
                    let ready = &ready;
                    waiters.push(Some(scope.spawn(move || {
                        let wake = Parker::current();
                        catch_unwind(AssertUnwindSafe(|| {
                            // SAFETY: The scope retains source and wake storage;
                            // all exits return or unwind through node cleanup.
                            unsafe {
                                queue.wait_until(
                                    &wake,
                                    || {
                                        entered[index].store(true, Ordering::Release);
                                        let is_ready = ready[index].load(Ordering::Acquire);
                                        if is_ready && index == removed && unwind {
                                            panic!("remove selected waiter");
                                        }
                                        is_ready
                                    },
                                    || {
                                        if index == removed {
                                            thread::park();
                                        } else {
                                            wake.park();
                                        }
                                    },
                                )
                            }
                        }))
                        .is_err()
                    })));
                    while !entered[index].load(Ordering::Acquire) {
                        thread::yield_now();
                    }
                }
                ready[removed].store(true, Ordering::Release);
                let selected = waiters[removed].take().unwrap();
                selected.thread().unpark();
                let did_unwind = selected.join().unwrap();
                for flag in &ready {
                    flag.store(true, Ordering::Release);
                }
                let notified = queue.notify_all();
                let others: Vec<_> = waiters
                    .into_iter()
                    .flatten()
                    .map(|waiter| waiter.join().unwrap())
                    .collect();
                assert_eq!(did_unwind, unwind);
                assert_eq!(notified, 2);
                assert_eq!(others, [false, false]);
                assert_eq!(queue.notify_all(), 0);
            });
        }
    }
}

#[test]
fn notification_racing_scope_exit_never_retains_stack_nodes() {
    for _ in 0..64 {
        let queue = WaitQueue::new();
        let entered = AtomicBool::new(false);
        let calls = AtomicUsize::new(0);
        struct BorrowedCounter<'a>(&'a AtomicUsize);
        // SAFETY: Only an atomic increment, without allocation, blocking,
        // unwinding, source reentry, or retention of borrowed storage.
        unsafe impl SyncWake for BorrowedCounter<'_> {
            fn notify(&self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        thread::scope(|scope| {
            let waiter = scope.spawn(|| {
                let wake = BorrowedCounter(&calls);
                // SAFETY: The scope retains the source and callback counter;
                // the local wake is dropped only after normal node cleanup.
                unsafe {
                    queue.wait_until(
                        &wake,
                        || {
                            entered.store(true, Ordering::Release);
                            true
                        },
                        || panic!("ready predicate must not park"),
                    )
                }
            });
            while !entered.load(Ordering::Acquire) {
                thread::yield_now();
            }
            let notified = queue.notify_all();
            waiter.join().unwrap();
            assert!(notified <= 1);
            assert_eq!(calls.load(Ordering::Relaxed), notified);
            assert_eq!(queue.notify_all(), 0);
        });
    }
}

#[test]
fn competing_consumers_cannot_duplicate_tokens() {
    for tokens in [0, 1, 3, 8] {
        let completion = Completion::new();
        for _ in 0..tokens {
            completion.complete();
        }
        let start = std::sync::Barrier::new(9);
        thread::scope(|scope| {
            let waiters: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        start.wait();
                        let mut consumed = false;
                        let counts = measure(|| consumed = completion.try_wait());
                        (consumed, counts)
                    })
                })
                .collect();
            start.wait();
            let mut consumed = 0;
            for waiter in waiters {
                let (succeeded, counts) = waiter.join().unwrap();
                assert_eq!(counts, (0, 0));
                consumed += usize::from(succeeded);
            }
            assert_eq!(consumed, tokens);
            assert!(!completion.try_wait());
        });
    }
}

#[test]
fn completion_publishes_payload_without_an_external_release() {
    let completion = Completion::new();
    let payload = AtomicUsize::new(0);
    let entered = AtomicBool::new(false);
    thread::scope(|scope| {
        let waiter = scope.spawn(|| {
            let wake = Parker::current();
            // SAFETY: All storage remains within the joined scope; the parker
            // returns normally and never abandons the registered stack node.
            unsafe {
                completion.wait_with(&wake, || {
                    entered.store(true, Ordering::Release);
                    wake.park();
                })
            };
            payload.load(Ordering::Relaxed)
        });
        while !entered.load(Ordering::Acquire) {
            thread::yield_now();
        }
        payload.store(0xc0ffee, Ordering::Relaxed);
        completion.complete();
        assert_eq!(waiter.join().unwrap(), 0xc0ffee);
    });
}

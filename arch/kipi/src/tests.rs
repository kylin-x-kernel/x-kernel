// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![cfg(test)]

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use super::*;

#[test]
fn test_callback_is_send() {
    // Verify Callback implements Send
    fn assert_send<T: Send>() {}
    assert_send::<Callback>();
}

#[test]
fn test_multicast_callback_is_send_sync() {
    // Verify MulticastCallback implements Send + Sync
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<MulticastCallback>();
}

#[test]
fn test_callback_creation() {
    let executed = Arc::new(AtomicUsize::new(0));
    let executed_clone = executed.clone();

    let callback = Callback::new(move || {
        executed_clone.fetch_add(1, Ordering::SeqCst);
    });

    callback.call();
    assert_eq!(executed.load(Ordering::SeqCst), 1);
}

#[test]
fn test_multicast_callback_clone() {
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_clone = counter.clone();

    let callback = MulticastCallback::new(move || {
        counter_clone.fetch_add(1, Ordering::SeqCst);
    });

    let callback2 = callback.clone();
    callback.call();
    callback2.call();

    assert_eq!(counter.load(Ordering::SeqCst), 2);
}

#[test]
fn test_queue_fifo() {
    let mut queue = IpiEventQueue::new();

    queue.push(0, Callback::new(|| {}));
    queue.push(1, Callback::new(|| {}));
    queue.push(2, Callback::new(|| {}));

    let (src1, _) = queue.pop_one().unwrap();
    let (src2, _) = queue.pop_one().unwrap();
    let (src3, _) = queue.pop_one().unwrap();

    assert_eq!(src1, 0);
    assert_eq!(src2, 1);
    assert_eq!(src3, 2);
}

#[test]
fn test_queue_empty() {
    let mut queue = IpiEventQueue::new();
    assert!(queue.is_empty());

    queue.push(0, Callback::new(|| {}));
    assert!(!queue.is_empty());

    let _ = queue.pop_one();
    assert!(queue.is_empty());
}

#[test]
fn test_sequential_callback_execution() {
    // Test that multiple callbacks execute in FIFO order and modify shared state correctly
    let execution_order = Arc::new(kspin::SpinNoIrq::new(alloc::vec::Vec::new()));
    let mut queue = IpiEventQueue::new();

    // Enqueue 3 callbacks that record their execution order
    for i in 0..3 {
        let order = execution_order.clone();
        queue.push(
            i,
            Callback::new(move || {
                order.lock().push(i);
            }),
        );
    }

    // Execute all callbacks
    while let Some((_, callback)) = queue.pop_one() {
        callback.call();
    }

    // Verify execution order matches enqueue order
    let final_order = execution_order.lock();
    assert_eq!(*final_order, alloc::vec![0, 1, 2]);
}

#[test]
fn test_callback_captures_and_modifies_state() {
    // Test that callbacks can capture and modify complex state
    let state = Arc::new(kspin::SpinNoIrq::new((
        0usize,
        alloc::string::String::new(),
    )));
    let state_clone = state.clone();

    let callback = Callback::new(move || {
        let mut s = state_clone.lock();
        s.0 += 42;
        s.1.push_str("executed");
    });

    callback.call();

    let final_state = state.lock();
    assert_eq!(final_state.0, 42);
    assert_eq!(final_state.1, "executed");
}

#[test]
fn test_multicast_callback_shared_state() {
    // Test that MulticastCallback can be called multiple times and share state
    let call_count = Arc::new(core::sync::atomic::AtomicUsize::new(0));
    let sum = Arc::new(core::sync::atomic::AtomicUsize::new(0));

    let count_clone = call_count.clone();
    let sum_clone = sum.clone();

    let callback = MulticastCallback::new(move || {
        let current = count_clone.fetch_add(1, Ordering::SeqCst);
        sum_clone.fetch_add(current * 10, Ordering::SeqCst);
    });

    // Simulate broadcast to 4 CPUs
    for _ in 0..4 {
        callback.clone().call();
    }

    assert_eq!(call_count.load(Ordering::SeqCst), 4);
    // Sum should be 0*10 + 1*10 + 2*10 + 3*10 = 60
    assert_eq!(sum.load(Ordering::SeqCst), 60);
}

#[test]
fn test_queue_interleaved_push_pop() {
    // Test queue behavior with interleaved push and pop operations
    let mut queue = IpiEventQueue::new();
    let results = Arc::new(kspin::SpinNoIrq::new(alloc::vec::Vec::new()));

    // Push 2, pop 1, push 2, pop 2, push 1
    queue.push(
        0,
        Callback::new({
            let r = results.clone();
            move || r.lock().push(100)
        }),
    );
    queue.push(
        1,
        Callback::new({
            let r = results.clone();
            move || r.lock().push(101)
        }),
    );

    // Pop first callback
    if let Some((src, cb)) = queue.pop_one() {
        assert_eq!(src, 0);
        cb.call();
    }

    // Push more
    queue.push(
        2,
        Callback::new({
            let r = results.clone();
            move || r.lock().push(102)
        }),
    );
    queue.push(
        3,
        Callback::new({
            let r = results.clone();
            move || r.lock().push(103)
        }),
    );

    // Pop remaining
    while let Some((_, cb)) = queue.pop_one() {
        cb.call();
    }

    let final_results = results.lock();
    assert_eq!(*final_results, alloc::vec![100, 101, 102, 103]);
}

#[test]
fn test_callback_with_complex_closure() {
    // Test callback with complex logic and multiple captured variables
    let multiplier = 3;
    let base = Arc::new(AtomicUsize::new(10));
    let result = Arc::new(AtomicUsize::new(0));

    let base_clone = base.clone();
    let result_clone = result.clone();

    let callback = Callback::new(move || {
        let b = base_clone.load(Ordering::SeqCst);
        let computed = b * multiplier + 7;
        result_clone.store(computed, Ordering::SeqCst);
    });

    callback.call();
    assert_eq!(result.load(Ordering::SeqCst), 37); // 10 * 3 + 7
}

#[test]
fn test_multicast_to_unicast_conversion() {
    // Test that multicast callback can be converted to unicast and still work
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_clone = counter.clone();

    let multicast = MulticastCallback::new(move || {
        counter_clone.fetch_add(1, Ordering::SeqCst);
    });

    // Convert to unicast and execute
    let unicast1 = multicast.clone().into_unicast();
    let unicast2 = multicast.clone().into_unicast();

    unicast1.call();
    unicast2.call();

    assert_eq!(counter.load(Ordering::SeqCst), 2);
}

#[test]
fn test_queue_pop_empty_returns_none() {
    // Test that popping from empty queue returns None consistently
    let mut queue = IpiEventQueue::new();

    assert!(queue.pop_one().is_none());
    assert!(queue.pop_one().is_none());

    queue.push(0, Callback::new(|| {}));
    assert!(queue.pop_one().is_some());
    assert!(queue.pop_one().is_none());
    assert!(queue.pop_one().is_none());
}

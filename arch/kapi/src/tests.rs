#![cfg(test)]

use super::*;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

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
fn test_error_display() {
    let err = KapiError::InvalidCpuId;
    assert_eq!(format!("{}", err), "Invalid CPU ID");
    
    let err = KapiError::QueueFull;
    assert_eq!(format!("{}", err), "IPI queue full");
    
    let err = KapiError::CallbackFailed;
    assert_eq!(format!("{}", err), "Callback execution failed");
}

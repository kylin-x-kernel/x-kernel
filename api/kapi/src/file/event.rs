//! Eventfd-backed file implementation.

use alloc::{borrow::Cow, sync::Arc};
use core::{
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    task::Context,
};

use kerrno::KError;
use kpoll::{IoEvents, PollSet, Pollable};
use ktask::future::{block_on, poll_io};

use crate::file::{FileLike, IoDst, IoSrc};

/// Kernel object implementing eventfd semantics.
///
/// - `count` is the current counter value.
/// - `semaphore` consumes 1 per read when true; otherwise read consumes all.
/// - `non_blocking` returns `WouldBlock` when the resource is unavailable.
pub struct EventFd {
    /// Current counter value.
    count: AtomicU64,
    /// Whether to read with semaphore semantics.
    semaphore: bool,
    /// Whether non-blocking mode is enabled.
    non_blocking: AtomicBool,

    /// Poll set for read side (waits for readable).
    poll_rx: PollSet,
    /// Poll set for write side (waits for writable).
    poll_tx: PollSet,
}

impl EventFd {
    /// Create a new eventfd object.
    ///
    /// - `initval` is the initial counter value.
    /// - `semaphore` makes each read decrement by 1 when true.
    pub fn new(initval: u64, semaphore: bool) -> Arc<Self> {
        Arc::new(Self {
            count: AtomicU64::new(initval),
            semaphore,
            non_blocking: AtomicBool::new(false),

            poll_rx: PollSet::new(),
            poll_tx: PollSet::new(),
        })
    }
}

impl FileLike for EventFd {
    /// Read the counter value.
    ///
    /// - Normal mode: return current count and clear it.
    /// - Semaphore mode: return current count and decrement by 1.
    fn read(&self, dst: &mut IoDst) -> kio::Result<usize> {
        if dst.remaining_mut() < size_of::<u64>() {
            return Err(KError::InvalidInput);
        }

        // Wait for readable when count is 0 (or return WouldBlock in non-blocking mode).
        block_on(poll_io(self, IoEvents::IN, self.nonblocking(), || {
            let result = self
                .count
                .fetch_update(Ordering::Release, Ordering::Acquire, |count| {
                    if count > 0 {
                        let dec = if self.semaphore { 1 } else { count };
                        Some(count - dec)
                    } else {
                        None
                    }
                });
            match result {
                Ok(count) => {
                    // Return the read value (note: this is the pre-update count).
                    dst.write(&count.to_ne_bytes())?;
                    self.poll_tx.wake();
                    Ok(size_of::<u64>())
                }
                Err(_) => Err(KError::WouldBlock),
            }
        }))
    }

    /// Write a value into the counter.
    ///
    /// - Valid range: 0..=u64::MAX-1.
    /// - Overflow returns `WouldBlock` (or returns immediately in non-blocking mode).
    fn write(&self, src: &mut IoSrc) -> kio::Result<usize> {
        if src.remaining() < size_of::<u64>() {
            return Err(KError::InvalidInput);
        }

        let mut value = [0; size_of::<u64>()];
        src.read(&mut value)?;
        let value = u64::from_ne_bytes(value);
        if value == u64::MAX {
            return Err(KError::InvalidInput);
        }

        // Wait for writable when close to max (or return WouldBlock in non-blocking mode).
        block_on(poll_io(self, IoEvents::OUT, self.nonblocking(), || {
            let result = self
                .count
                .fetch_update(Ordering::Release, Ordering::Acquire, |count| {
                    if u64::MAX - count > value {
                        Some(count + value)
                    } else {
                        None
                    }
                });
            match result {
                Ok(_) => {
                    self.poll_rx.wake();
                    Ok(size_of::<u64>())
                }
                Err(_) => Err(KError::WouldBlock),
            }
        }))
    }

    fn nonblocking(&self) -> bool {
        self.non_blocking.load(Ordering::Acquire)
    }

    /// Set non-blocking mode.
    fn set_nonblocking(&self, non_blocking: bool) -> kio::Result {
        self.non_blocking.store(non_blocking, Ordering::Release);
        Ok(())
    }

    /// Return the anonymous inode path (matches Linux eventfd behavior).
    fn path(&self) -> Cow<'_, str> {
        "anon_inode:[eventfd]".into()
    }
}

impl Pollable for EventFd {
    /// Generate readable/writable events from current count.
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        let count = self.count.load(Ordering::Acquire);
        events.set(IoEvents::IN, count > 0);
        events.set(IoEvents::OUT, u64::MAX - 1 > count);
        events
    }

    /// Register current task wakers for the requested events.
    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        if events.contains(IoEvents::IN) {
            self.poll_rx.register(context.waker());
        }
        if events.contains(IoEvents::OUT) {
            self.poll_tx.register(context.waker());
        }
    }
}

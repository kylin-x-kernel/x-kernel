// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `eventfd` object implementation.

use alloc::{borrow::Cow, sync::Arc};
use core::{
    mem::size_of,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    task::Context,
};

use kerrno::KError;
use kfd::{FileLike, IoDst, IoSrc};
use kpoll::{IoEvents, PollSet, Pollable};
use ktask::future::{block_on, poll_io};

/// Kernel object implementing eventfd semantics.
pub struct EventFd {
    count: AtomicU64,
    semaphore: bool,
    non_blocking: AtomicBool,
    poll_rx: PollSet,
    poll_tx: PollSet,
}

impl EventFd {
    /// Create a new eventfd object.
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
    fn read(&self, dst: &mut IoDst) -> kio::Result<usize> {
        if dst.remaining_mut() < size_of::<u64>() {
            return Err(KError::InvalidInput);
        }

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
                    dst.write(&count.to_ne_bytes())?;
                    self.poll_tx.wake();
                    Ok(size_of::<u64>())
                }
                Err(_) => Err(KError::WouldBlock),
            }
        }))
    }

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

    fn set_nonblocking(&self, non_blocking: bool) -> kio::Result {
        self.non_blocking.store(non_blocking, Ordering::Release);
        Ok(())
    }

    fn path(&self) -> Cow<'_, str> {
        "anon_inode:[eventfd]".into()
    }
}

impl Pollable for EventFd {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        let count = self.count.load(Ordering::Acquire);
        events.set(IoEvents::IN, count > 0);
        events.set(IoEvents::OUT, u64::MAX - 1 > count);
        events
    }

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        if events.contains(IoEvents::IN) {
            self.poll_rx.register(context.waker());
        }
        if events.contains(IoEvents::OUT) {
            self.poll_tx.register(context.waker());
        }
    }
}

#[cfg(unittest)]
mod eventfd_tests {
    use core::task::{Context, RawWaker, Waker};

    use kpoll::IoEvents;
    use unittest::def_test;

    use super::*;

    #[def_test]
    fn test_eventfd_creation() {
        let eventfd = EventFd::new(0, false);
        assert_eq!(eventfd.path(), "anon_inode:[eventfd]");
    }

    #[def_test]
    fn test_eventfd_with_initval() {
        let eventfd = EventFd::new(42, false);
        assert!(eventfd.poll().contains(IoEvents::IN));
        assert!(eventfd.poll().contains(IoEvents::OUT));
    }

    #[def_test]
    fn test_eventfd_poll_states() {
        let eventfd = EventFd::new(0, false);
        let events = eventfd.poll();
        assert!(!events.contains(IoEvents::IN));
        assert!(events.contains(IoEvents::OUT));

        let eventfd = EventFd::new(1, false);
        let events = eventfd.poll();
        assert!(events.contains(IoEvents::IN));
        assert!(events.contains(IoEvents::OUT));

        let eventfd = EventFd::new(u64::MAX - 1, false);
        let events = eventfd.poll();
        assert!(events.contains(IoEvents::IN));
        assert!(!events.contains(IoEvents::OUT));
    }

    #[def_test]
    fn test_eventfd_semaphore_mode() {
        let eventfd = EventFd::new(10, true);
        assert_eq!(eventfd.path(), "anon_inode:[eventfd]");
    }

    #[def_test]
    fn test_eventfd_nonblocking_mode() {
        let eventfd = EventFd::new(0, false);

        assert!(!eventfd.nonblocking());

        eventfd.set_nonblocking(true).unwrap();
        assert!(eventfd.nonblocking());

        eventfd.set_nonblocking(false).unwrap();
        assert!(!eventfd.nonblocking());
    }

    #[def_test]
    fn test_eventfd_poll_at_max() {
        let eventfd = EventFd::new(u64::MAX, false);
        let events = eventfd.poll();
        assert!(events.contains(IoEvents::IN));
        assert!(!events.contains(IoEvents::OUT));
    }

    #[def_test]
    fn test_eventfd_poll_near_max() {
        let eventfd = EventFd::new(u64::MAX - 1, false);
        let events = eventfd.poll();
        assert!(events.contains(IoEvents::IN));
        assert!(!events.contains(IoEvents::OUT));
    }

    #[def_test]
    fn test_eventfd_poll_can_write_one() {
        let eventfd = EventFd::new(u64::MAX - 2, false);
        let events = eventfd.poll();
        assert!(events.contains(IoEvents::IN));
        assert!(events.contains(IoEvents::OUT));
    }

    #[def_test]
    fn test_eventfd_register() {
        static VTABLE: core::task::RawWakerVTable = core::task::RawWakerVTable::new(
            |_| RawWaker::new(core::ptr::null(), &VTABLE),
            |_| {},
            |_| {},
            |_| {},
        );

        let eventfd = EventFd::new(0, false);
        let raw_waker = RawWaker::new(core::ptr::null(), &VTABLE);
        // SAFETY: The dummy raw waker uses no-op callbacks and a null data pointer that
        // is never dereferenced. It is used only to exercise registration paths in tests.
        let waker = unsafe { Waker::from_raw(raw_waker) };
        let mut context = Context::from_waker(&waker);

        eventfd.register(&mut context, IoEvents::IN);
        eventfd.register(&mut context, IoEvents::OUT);
        eventfd.register(&mut context, IoEvents::IN | IoEvents::OUT);
    }

    #[def_test]
    fn test_eventfd_path_consistency() {
        let eventfd1 = EventFd::new(0, false);
        let eventfd2 = EventFd::new(100, true);
        let eventfd3 = EventFd::new(u64::MAX, false);

        assert_eq!(eventfd1.path(), "anon_inode:[eventfd]");
        assert_eq!(eventfd2.path(), "anon_inode:[eventfd]");
        assert_eq!(eventfd3.path(), "anon_inode:[eventfd]");
    }

    #[def_test]
    fn test_eventfd_max_initval() {
        let eventfd = EventFd::new(u64::MAX, false);
        let events = eventfd.poll();
        assert!(events.contains(IoEvents::IN));
        assert!(!events.contains(IoEvents::OUT));
    }

    #[def_test]
    fn test_eventfd_write_then_read() {
        let eventfd = EventFd::new(0, false);

        let data = 42u64.to_ne_bytes();
        let mut src = kio::Cursor::new(data.as_slice());
        let mut dst_buf = [0; size_of::<u64>()];
        let mut dst = kio::Cursor::new(dst_buf.as_mut_slice());

        assert_eq!(eventfd.write(&mut src).unwrap(), size_of::<u64>());
        assert!(eventfd.poll().contains(IoEvents::IN));

        assert_eq!(eventfd.read(&mut dst).unwrap(), size_of::<u64>());
        assert_eq!(u64::from_ne_bytes(dst_buf), 42);
        assert!(!eventfd.poll().contains(IoEvents::IN));
    }

    #[def_test]
    fn test_eventfd_semaphore_read_keeps_remaining_count() {
        let eventfd = EventFd::new(3, true);
        let mut dst_buf = [0; size_of::<u64>()];
        let mut dst = kio::Cursor::new(dst_buf.as_mut_slice());

        assert_eq!(eventfd.read(&mut dst).unwrap(), size_of::<u64>());
        assert_eq!(u64::from_ne_bytes(dst_buf), 3);

        let events = eventfd.poll();
        assert!(events.contains(IoEvents::IN));
        assert!(events.contains(IoEvents::OUT));
    }

    #[def_test]
    fn test_eventfd_invalid_write_value() {
        let eventfd = EventFd::new(0, false);
        let data = u64::MAX.to_ne_bytes();
        let mut src = kio::Cursor::new(data.as_slice());

        assert_eq!(eventfd.write(&mut src), Err(KError::InvalidInput));
    }

    #[def_test]
    fn test_eventfd_small_buffers_fail() {
        let eventfd = EventFd::new(1, false);

        let mut short_dst = [0; size_of::<u64>() - 1];
        let mut dst = kio::Cursor::new(short_dst.as_mut_slice());
        assert_eq!(eventfd.read(&mut dst), Err(KError::InvalidInput));

        let short_src = [0; size_of::<u64>() - 1];
        let mut src = kio::Cursor::new(short_src.as_slice());
        assert_eq!(eventfd.write(&mut src), Err(KError::InvalidInput));
    }
}

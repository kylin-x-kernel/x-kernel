// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `eventfd` object implementation.

use alloc::sync::Arc;
use core::{
    mem::size_of,
    sync::atomic::{AtomicU64, Ordering},
};

use anon_inodefs::AnonInodeFs;
use kcred::Cred;
use kerrno::{KError, KResult};
use kpoll::{IoEvents, PollContext, PollRegisterError, PollSet, Pollable};
use ktask::future::block_on;
use kvfs::{FMode, FileOperations, OpenFlags, VfsFile, poll_file_io};

/// Kernel object implementing eventfd semantics.
pub struct EventFd {
    count: AtomicU64,
    semaphore: bool,
    poll_rx: PollSet,
    poll_tx: PollSet,
}

impl EventFd {
    /// Creates a counter object with the supplied initial value and read mode.
    ///
    /// Semaphore mode decrements by one; the current file read implementation
    /// returns the pre-update counter in either mode. The syscall adapter
    /// supplies the ABI-bounded initial value.
    pub fn new(initval: u64, semaphore: bool) -> Arc<Self> {
        Arc::new(Self {
            count: AtomicU64::new(initval),
            semaphore,
            poll_rx: PollSet::new(),
            poll_tx: PollSet::new(),
        })
    }

    /// Create the anonymous-inode file used by eventfd.
    ///
    /// `cred` is captured as the new file's immutable open credential.
    ///
    /// # Errors
    ///
    /// Returns `KError::InvalidInput` for unknown open-flag bits and forwards
    /// errors from anonymous-inode file creation.
    pub fn new_file(
        initval: u64,
        semaphore: bool,
        open_flags: u32,
        cred: Arc<Cred>,
    ) -> KResult<Arc<VfsFile>> {
        let state = Self::new(initval, semaphore);
        let open_flags = OpenFlags::from_bits(open_flags).ok_or(KError::InvalidInput)?;
        AnonInodeFs::global().get_file(
            "[eventfd]",
            Arc::new(EventfdFops),
            state,
            FMode::READ | FMode::WRITE | FMode::STREAM,
            open_flags,
            cred,
        )
    }

    /// Returns the eventfd object attached to an eventfd file.
    ///
    /// # Errors
    ///
    /// Returns `KError::BadFileDescriptor` when the file has no matching private data.
    pub fn from_file(file: &VfsFile) -> KResult<Arc<Self>> {
        file.private_data_get::<Self>()
            .ok_or(KError::BadFileDescriptor)
    }
}

struct EventfdFops;

impl EventfdFops {
    fn state(file: &VfsFile) -> kio::Result<Arc<EventFd>> {
        EventFd::from_file(file)
    }

    async fn read_wait(file: &VfsFile, buf: &mut [u8]) -> kio::Result<usize> {
        let state = Self::state(file)?;
        if buf.len() < size_of::<u64>() {
            return Err(KError::InvalidInput);
        }

        poll_file_io(state.as_ref(), IoEvents::IN, file.is_nonblocking(), || {
            let result = state
                .count
                .fetch_update(Ordering::Release, Ordering::Acquire, |count| {
                    if count > 0 {
                        let dec = if state.semaphore { 1 } else { count };
                        Some(count - dec)
                    } else {
                        None
                    }
                });

            match result {
                Ok(count) => {
                    buf[..size_of::<u64>()].copy_from_slice(&count.to_ne_bytes());
                    state.poll_tx.wake();
                    Ok(size_of::<u64>())
                }
                Err(_) => Err(KError::WouldBlock),
            }
        })
        .await
    }

    async fn write_wait(file: &VfsFile, buf: &[u8]) -> kio::Result<usize> {
        let state = Self::state(file)?;
        if buf.len() < size_of::<u64>() {
            return Err(KError::InvalidInput);
        }

        let mut value = [0; size_of::<u64>()];
        value.copy_from_slice(&buf[..size_of::<u64>()]);
        let value = u64::from_ne_bytes(value);
        if value == u64::MAX {
            return Err(KError::InvalidInput);
        }

        poll_file_io(state.as_ref(), IoEvents::OUT, file.is_nonblocking(), || {
            let result = state
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
                    state.poll_rx.wake();
                    Ok(size_of::<u64>())
                }
                Err(_) => Err(KError::WouldBlock),
            }
        })
        .await
    }
}

impl FileOperations for EventfdFops {
    fn supports_read(&self) -> bool {
        true
    }

    fn supports_write(&self) -> bool {
        true
    }

    fn read(&self, file: &VfsFile, buf: &mut [u8], _offset: u64) -> kio::Result<usize> {
        block_on(Self::read_wait(file, buf))
    }

    fn write(&self, file: &VfsFile, buf: &[u8], _offset: u64) -> kio::Result<usize> {
        block_on(Self::write_wait(file, buf))
    }

    fn poll(&self, file: &VfsFile) -> IoEvents {
        Self::state(file)
            .map(|state| state.poll())
            .unwrap_or_else(|_| IoEvents::empty())
    }

    fn register_poll(
        &self,
        file: &VfsFile,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if let Ok(state) = Self::state(file) {
            state.register(context, events)?;
        }
        Ok(())
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

    fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if events.contains(IoEvents::IN) {
            context.register(&self.poll_rx)?;
        }
        if events.contains(IoEvents::OUT) {
            context.register(&self.poll_tx)?;
        }
        Ok(())
    }
}

#[unittest::mod_test]
mod eventfd_tests {
    use core::task::{Context, RawWaker, Waker};

    use kpoll::{IoEvents, PollRegistrations};
    use unittest::def_test;

    use super::*;

    #[def_test]
    fn test_eventfd_creation() {
        let eventfd = EventFd::new(0, false);
        assert!(!eventfd.poll().contains(IoEvents::IN));
        assert!(eventfd.poll().contains(IoEvents::OUT));
    }

    #[def_test]
    fn eventfd_wait_interruption_does_not_override_ready_or_nonblocking_io() {
        let file = EventFd::new_file(0, false, 0, kcred::initial_cred()).unwrap();
        let task = ktask::current();
        let mut bytes = [0; 8];
        let mut pos = 0;
        task.interrupt();
        let blocked = file.read_from(&mut bytes, &mut pos);
        task.clear_interrupt();
        assert_eq!(blocked, Err(KError::IoWaitInterrupted));

        file.set_nonblocking(true);
        task.interrupt();
        let empty = file.read_from(&mut bytes, &mut pos);
        let preserved = task.is_interrupted();
        task.clear_interrupt();
        assert_eq!(empty, Err(KError::WouldBlock));
        assert!(preserved);

        task.interrupt();
        let write = file.write_from(&7u64.to_ne_bytes(), &mut pos);
        let read = file.read_from(&mut bytes, &mut pos);
        let preserved = task.is_interrupted();
        task.clear_interrupt();
        assert_eq!(write, Ok(8));
        assert_eq!(read, Ok(8));
        assert_eq!(u64::from_ne_bytes(bytes), 7);
        assert!(preserved);

        file.set_nonblocking(false);
        assert_eq!(
            file.write_from(&(u64::MAX - 1).to_ne_bytes(), &mut pos),
            Ok(8)
        );
        task.interrupt();
        let full = file.write_from(&1u64.to_ne_bytes(), &mut pos);
        task.clear_interrupt();
        assert_eq!(full, Err(KError::IoWaitInterrupted));
        assert_eq!(file.read_from(&mut bytes, &mut pos), Ok(8));
        assert_eq!(u64::from_ne_bytes(bytes), u64::MAX - 1);
    }

    #[def_test]
    fn eventfd_pending_read_and_write_recheck_ready_state_after_signal() {
        use alloc::boxed::Box;
        use core::{
            future::Future,
            task::{Context, Poll, Waker},
        };

        struct ClearInterrupt;
        impl Drop for ClearInterrupt {
            fn drop(&mut self) {
                ktask::current().clear_interrupt();
            }
        }
        let _cleanup = ClearInterrupt;
        let task = ktask::current();
        task.clear_interrupt();
        let file = EventFd::new_file(0, false, 0, kcred::initial_cred()).unwrap();
        let state = EventFd::from_file(&file).unwrap();
        let mut cx = Context::from_waker(Waker::noop());
        let mut bytes = [0; 8];
        let mut read = Box::pin(EventfdFops::read_wait(&file, &mut bytes));
        assert_eq!(read.as_mut().poll(&mut cx), Poll::Pending);
        assert_eq!(EventfdFops.write(&file, &7u64.to_ne_bytes(), 0), Ok(8));
        task.interrupt();
        assert_eq!(read.as_mut().poll(&mut cx), Poll::Ready(Ok(8)));
        drop(read);
        assert_eq!(u64::from_ne_bytes(bytes), 7);
        assert_eq!(state.count.load(Ordering::Acquire), 0);
        task.clear_interrupt();

        assert_eq!(
            EventfdFops.write(&file, &(u64::MAX - 1).to_ne_bytes(), 0),
            Ok(8)
        );
        let increment = 3u64.to_ne_bytes();
        let mut write = Box::pin(EventfdFops::write_wait(&file, &increment));
        assert_eq!(write.as_mut().poll(&mut cx), Poll::Pending);
        assert_eq!(EventfdFops.read(&file, &mut bytes, 0), Ok(8));
        assert_eq!(u64::from_ne_bytes(bytes), u64::MAX - 1);
        task.interrupt();
        assert_eq!(write.as_mut().poll(&mut cx), Poll::Ready(Ok(8)));
        drop(write);
        assert_eq!(state.count.load(Ordering::Acquire), 3);
        assert_eq!(EventfdFops.read(&file, &mut bytes, 0), Ok(8));
        assert_eq!(u64::from_ne_bytes(bytes), 3);
        assert_eq!(state.count.load(Ordering::Acquire), 0);
        task.clear_interrupt();
        let mut read = Box::pin(EventfdFops::read_wait(&file, &mut bytes));
        assert_eq!(read.as_mut().poll(&mut cx), Poll::Pending);
        task.interrupt();
        assert_eq!(
            read.as_mut().poll(&mut cx),
            Poll::Ready(Err(KError::IoWaitInterrupted))
        );
        drop(read);
        assert_eq!(state.count.load(Ordering::Acquire), 0);
        task.clear_interrupt();
        assert_eq!(
            EventfdFops.write(&file, &(u64::MAX - 1).to_ne_bytes(), 0),
            Ok(8)
        );
        let mut write = Box::pin(EventfdFops::write_wait(&file, &increment));
        assert_eq!(write.as_mut().poll(&mut cx), Poll::Pending);
        task.interrupt();
        assert_eq!(
            write.as_mut().poll(&mut cx),
            Poll::Ready(Err(KError::IoWaitInterrupted))
        );
        drop(write);
        assert_eq!(state.count.load(Ordering::Acquire), u64::MAX - 1);
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
        assert!(eventfd.poll().contains(IoEvents::IN));
    }

    #[def_test]
    fn test_eventfd_nonblocking_mode() {
        let file =
            EventFd::new_file(0, false, 0, kcred::initial_cred()).expect("eventfd file opens");

        assert!(!file.is_nonblocking());

        file.set_nonblocking(true);
        assert!(file.is_nonblocking());

        file.set_nonblocking(false);
        assert!(!file.is_nonblocking());
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
        let context = Context::from_waker(&waker);
        let mut registrations = PollRegistrations::new();
        let mut context = registrations.context(&context);

        eventfd.register(&mut context, IoEvents::IN).unwrap();
        eventfd.register(&mut context, IoEvents::OUT).unwrap();
        eventfd
            .register(&mut context, IoEvents::IN | IoEvents::OUT)
            .unwrap();
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
        let file =
            EventFd::new_file(0, false, 0, kcred::initial_cred()).expect("eventfd file opens");

        let data = 42u64.to_ne_bytes();
        let mut dst_buf = [0; size_of::<u64>()];
        let mut pos = 0;

        assert_eq!(file.write_from(&data, &mut pos).unwrap(), size_of::<u64>());
        assert!(file.poll().contains(IoEvents::IN));

        assert_eq!(
            file.read_from(&mut dst_buf, &mut pos).unwrap(),
            size_of::<u64>()
        );
        assert_eq!(u64::from_ne_bytes(dst_buf), 42);
        assert!(!file.poll().contains(IoEvents::IN));
    }

    #[def_test]
    fn test_eventfd_semaphore_read_keeps_remaining_count() {
        let file =
            EventFd::new_file(3, true, 0, kcred::initial_cred()).expect("eventfd file opens");
        let mut dst_buf = [0; size_of::<u64>()];
        let mut pos = 0;

        assert_eq!(
            file.read_from(&mut dst_buf, &mut pos).unwrap(),
            size_of::<u64>()
        );
        assert_eq!(u64::from_ne_bytes(dst_buf), 3);

        let events = file.poll();
        assert!(events.contains(IoEvents::IN));
        assert!(events.contains(IoEvents::OUT));
    }

    #[def_test]
    fn test_eventfd_invalid_write_value() {
        let file =
            EventFd::new_file(0, false, 0, kcred::initial_cred()).expect("eventfd file opens");
        let data = u64::MAX.to_ne_bytes();
        let mut pos = 0;

        assert_eq!(file.write_from(&data, &mut pos), Err(KError::InvalidInput));
    }

    #[def_test]
    fn test_eventfd_small_buffers_fail() {
        let file =
            EventFd::new_file(1, false, 0, kcred::initial_cred()).expect("eventfd file opens");
        let mut pos = 0;

        let mut short_dst = [0; size_of::<u64>() - 1];
        assert_eq!(
            file.read_from(&mut short_dst, &mut pos),
            Err(KError::InvalidInput)
        );

        let short_src = [0; size_of::<u64>() - 1];
        assert_eq!(
            file.write_from(&short_src, &mut pos),
            Err(KError::InvalidInput)
        );
    }
}

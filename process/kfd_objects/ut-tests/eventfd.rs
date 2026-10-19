// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

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
    let file = EventFd::new_file(0, false, 0, kcred::initial_cred()).expect("eventfd file opens");

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
    let file = EventFd::new_file(0, false, 0, kcred::initial_cred()).expect("eventfd file opens");

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
    let file = EventFd::new_file(3, true, 0, kcred::initial_cred()).expect("eventfd file opens");
    file.set_nonblocking(true);
    let state = EventFd::from_file(&file).unwrap();
    let mut bytes = [0; size_of::<u64>()];
    let mut pos = 0;

    for remaining in [2, 1, 0] {
        assert_eq!(file.read_from(&mut bytes, &mut pos), Ok(size_of::<u64>()));
        assert_eq!(u64::from_ne_bytes(bytes), 1);
        assert_eq!(state.count.load(Ordering::Acquire), remaining);
        assert_eq!(file.poll().contains(IoEvents::IN), remaining != 0);
        assert!(file.poll().contains(IoEvents::OUT));
    }
    bytes.fill(0xa5);
    assert_eq!(
        file.read_from(&mut bytes, &mut pos),
        Err(KError::WouldBlock)
    );
    assert_eq!(bytes, [0xa5; size_of::<u64>()]);

    assert_eq!(
        file.write_from(&2u64.to_ne_bytes(), &mut pos),
        Ok(size_of::<u64>())
    );
    for remaining in [1, 0] {
        assert_eq!(file.read_from(&mut bytes, &mut pos), Ok(size_of::<u64>()));
        assert_eq!(u64::from_ne_bytes(bytes), 1);
        assert_eq!(state.count.load(Ordering::Acquire), remaining);
    }
    assert_eq!(
        file.read_from(&mut bytes, &mut pos),
        Err(KError::WouldBlock)
    );
}

#[def_test]
fn eventfd_counter_mode_returns_accumulated_value_and_drains() {
    let file = EventFd::new_file(3, false, 0, kcred::initial_cred()).expect("eventfd file opens");
    file.set_nonblocking(true);
    let mut bytes = [0; size_of::<u64>()];
    let mut pos = 0;

    assert_eq!(file.read_from(&mut bytes, &mut pos), Ok(size_of::<u64>()));
    assert_eq!(u64::from_ne_bytes(bytes), 3);
    assert!(!file.poll().contains(IoEvents::IN));
    assert_eq!(
        file.read_from(&mut bytes, &mut pos),
        Err(KError::WouldBlock)
    );
    assert_eq!(
        file.write_from(&9u64.to_ne_bytes(), &mut pos),
        Ok(size_of::<u64>())
    );
    assert_eq!(file.read_from(&mut bytes, &mut pos), Ok(size_of::<u64>()));
    assert_eq!(u64::from_ne_bytes(bytes), 9);
    assert_eq!(
        file.read_from(&mut bytes, &mut pos),
        Err(KError::WouldBlock)
    );
}

#[def_test]
fn test_eventfd_invalid_write_value() {
    let file = EventFd::new_file(0, false, 0, kcred::initial_cred()).expect("eventfd file opens");
    let data = u64::MAX.to_ne_bytes();
    let mut pos = 0;

    assert_eq!(file.write_from(&data, &mut pos), Err(KError::InvalidInput));
}

#[def_test]
fn test_eventfd_small_buffers_fail() {
    let file = EventFd::new_file(1, false, 0, kcred::initial_cred()).expect("eventfd file opens");
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

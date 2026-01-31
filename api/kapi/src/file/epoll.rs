// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Epoll instance and interest management.
use alloc::{
    borrow::Cow,
    collections::vec_deque::VecDeque,
    sync::{Arc, Weak},
    task::Wake,
};
use core::{
    hash::{Hash, Hasher},
    sync::atomic::{AtomicBool, Ordering},
    task::{Context, Waker},
};

use bitflags::bitflags;
use hashbrown::HashMap;
use kerrno::{KError, KResult};
use kpoll::{IoEvents, PollSet, Pollable};
use kspin::SpinNoPreempt;
use linux_raw_sys::general::{EPOLLET, EPOLLONESHOT, epoll_event};

use crate::file::{FileLike, get_file_like};

pub struct EpollEvent {
    /// Interested I/O events.
    pub events: IoEvents,
    /// User data associated with the interest.
    pub user_data: u64,
}

bitflags! {
    /// Flags for the entries in the `epoll` instance.
    #[derive(Debug, Clone, Copy, Default)]
    pub struct EpollFlags: u32 {
        const EDGE_TRIGGER = EPOLLET;
        const ONESHOT = EPOLLONESHOT;
    }
}

/// Interest trigger mode
#[derive(Debug, Clone, Copy)]
enum TriggerMode {
    /// Level-triggered: until the condition is cleared
    Level,
    /// Edge-triggered: only notify when the condition changes
    Edge,
    /// One-shot: notify only once
    OneShot { fired: bool },
}

impl TriggerMode {
    fn from_flags(flags: EpollFlags) -> Self {
        if flags.contains(EpollFlags::ONESHOT) {
            TriggerMode::OneShot { fired: false }
        } else if flags.contains(EpollFlags::EDGE_TRIGGER) {
            TriggerMode::Edge
        } else {
            TriggerMode::Level
        }
    }

    // return should notify and new mode
    fn should_notify(&self) -> (bool, Self) {
        match self {
            TriggerMode::Level => {
                // LT: always notify
                (true, *self)
            }
            // if we could wake, we need notify
            TriggerMode::Edge => (true, TriggerMode::Edge),
            TriggerMode::OneShot { fired } => {
                // ONESHOT: 只触发一次
                if *fired {
                    (false, *self)
                } else {
                    (true, TriggerMode::OneShot { fired: true })
                }
            }
        }
    }

    fn is_enabled(&self) -> bool {
        match self {
            TriggerMode::OneShot { fired } => !fired,
            _ => true,
        }
    }
}

enum ConsumeResult {
    // success and should keep in ready list
    EventAndKeep(EpollEvent),
    // success and hould remove ready list
    EventAndRemove(EpollEvent),
    // no event and should remove ready list
    NoEvent,
}

#[derive(Clone)]
struct EntryKey {
    fd: i32,
    file: Weak<dyn FileLike>,
}
impl EntryKey {
    fn new(fd: i32) -> KResult<Self> {
        let file = get_file_like(fd)?;
        Ok(Self {
            fd,
            file: Arc::downgrade(&file),
        })
    }

    #[inline]
    fn get_file(&self) -> Option<Arc<dyn FileLike>> {
        self.file.upgrade()
    }
}

impl Hash for EntryKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (self.fd, self.file.as_ptr()).hash(state);
    }
}
impl PartialEq for EntryKey {
    fn eq(&self, other: &Self) -> bool {
        self.fd == other.fd && Weak::ptr_eq(&self.file, &other.file)
    }
}

impl Eq for EntryKey {}

struct EpollInterest {
    key: EntryKey,
    event: EpollEvent,
    mode: SpinNoPreempt<TriggerMode>,
    in_ready_queue: AtomicBool,
}

impl EpollInterest {
    fn new(key: EntryKey, event: EpollEvent, flags: EpollFlags) -> Self {
        Self {
            key,
            event,
            mode: SpinNoPreempt::new(TriggerMode::from_flags(flags)),
            in_ready_queue: AtomicBool::new(false),
        }
    }

    #[inline]
    fn is_enabled(&self) -> bool {
        self.mode.lock().is_enabled()
    }

    #[inline]
    fn is_in_queue(&self) -> bool {
        self.in_ready_queue.load(Ordering::Acquire)
    }

    #[inline]
    fn try_mark_in_queue(&self) -> bool {
        self.in_ready_queue
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    #[inline]
    fn mark_not_in_queue(&self) {
        self.in_ready_queue.store(false, Ordering::Release);
    }

    fn consume(&self, file: &dyn FileLike) -> ConsumeResult {
        let current_events = file.poll();
        let matched = current_events & self.event.events;

        // not ready
        if matched.is_empty() {
            return ConsumeResult::NoEvent;
        }

        let mut mode = self.mode.lock();
        let (should_notify, new_mode) = mode.should_notify();
        *mode = new_mode;
        trace!(
            "consume fd: {} matches {:?} should notify: {} ",
            self.key.fd, matched, should_notify
        );

        if !should_notify {
            return ConsumeResult::NoEvent;
        }

        // create event
        let event = EpollEvent {
            events: matched,
            user_data: self.event.user_data,
        };

        // shoud still keep in ready?
        match *mode {
            TriggerMode::Level => ConsumeResult::EventAndKeep(event),
            TriggerMode::Edge | TriggerMode::OneShot { .. } => ConsumeResult::EventAndRemove(event),
        }
    }
}

struct InterestWaker {
    epoll: Weak<EpollInner>,
    interest: Weak<EpollInterest>,
}

impl Wake for InterestWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        let Some(epoll) = self.epoll.upgrade() else {
            return;
        };

        let Some(interest) = self.interest.upgrade() else {
            return;
        };

        if interest.try_mark_in_queue() {
            epoll
                .ready_queue
                .lock()
                .push_back(Arc::downgrade(&interest));
            trace!(
                "Epoll: fd={} added to ready queue, events={:?} wake up poller",
                interest.key.fd, interest.event.events
            );
            epoll.poll_ready.wake();
        }
    }
}

struct EpollInner {
    interests: SpinNoPreempt<HashMap<EntryKey, Arc<EpollInterest>>>,
    ready_queue: SpinNoPreempt<VecDeque<Weak<EpollInterest>>>,
    poll_ready: PollSet,
}

impl Default for EpollInner {
    fn default() -> Self {
        Self {
            interests: SpinNoPreempt::new(HashMap::new()),
            ready_queue: SpinNoPreempt::new(VecDeque::new()),
            poll_ready: PollSet::new(),
        }
    }
}

#[derive(Default)]
pub struct Epoll {
    inner: Arc<EpollInner>,
}

impl Epoll {
    /// Creates a new epoll instance.
    pub fn new() -> Self {
        Self::default()
    }

    // only register waker, not add to ready queue
    fn register_waker_only(&self, interest: &Arc<EpollInterest>) {
        let Some(file) = interest.key.get_file() else {
            return;
        };

        if !interest.is_enabled() {
            return;
        }

        let waker = Waker::from(Arc::new(InterestWaker {
            epoll: Arc::downgrade(&self.inner),
            interest: Arc::downgrade(interest),
        }));

        let mut context = Context::from_waker(&waker);
        file.register(&mut context, interest.event.events);
    }

    // for add/modify
    fn check_and_register_waker(&self, interest: &Arc<EpollInterest>) {
        let Some(file) = interest.key.get_file() else {
            return;
        };

        if !interest.is_enabled() {
            return;
        }

        let waker = Waker::from(Arc::new(InterestWaker {
            epoll: Arc::downgrade(&self.inner),
            interest: Arc::downgrade(interest),
        }));

        let current = file.poll() & interest.event.events;

        if !current.is_empty() {
            waker.wake_by_ref();
        } else {
            let mut context = Context::from_waker(&waker);
            file.register(&mut context, interest.event.events);

            let current = file.poll() & interest.event.events;
            if !current.is_empty() {
                waker.wake_by_ref();
            }
        }
    }

    /// Adds a file descriptor interest to the epoll instance.
    pub fn add(&self, fd: i32, event: EpollEvent, flags: EpollFlags) -> KResult<()> {
        let key = EntryKey::new(fd)?;
        let interest = Arc::new(EpollInterest::new(key.clone(), event, flags));
        let mut guard = self.inner.interests.lock();
        if guard.contains_key(&key) {
            return Err(KError::AlreadyExists);
        }
        guard.insert(key.clone(), Arc::clone(&interest));
        drop(guard);
        trace!("Epoll add fd: {} interest {:?} ", fd, interest.event.events);
        self.check_and_register_waker(&interest);
        Ok(())
    }

    /// Modifies an existing interest for the given file descriptor.
    pub fn modify(&self, fd: i32, event: EpollEvent, flags: EpollFlags) -> KResult<()> {
        let key = EntryKey::new(fd)?;
        let interest = Arc::new(EpollInterest::new(key.clone(), event, flags));

        let mut guard = self.inner.interests.lock();
        let old = guard.get_mut(&key).ok_or(KError::NotFound)?;

        // update new interest if old already in ready queue
        if old.is_in_queue() {
            interest.in_ready_queue.store(true, Ordering::Release);
        }
        *old = Arc::clone(&interest);
        drop(guard);
        trace!(
            "Epoll: modify fd={}, events={:?}",
            fd, interest.event.events
        );
        // reset waker
        self.check_and_register_waker(&interest);
        Ok(())
    }

    /// Removes an existing interest for the given file descriptor.
    pub fn delete(&self, fd: i32) -> KResult<()> {
        let key = EntryKey::new(fd)?;
        self.inner
            .interests
            .lock()
            .remove(&key)
            .ok_or(KError::NotFound)?;
        trace!("Epoll: delete fd={fd}");
        Ok(())
    }

    /// Polls for ready events and writes them into `out`.
    pub fn poll_events(&self, out: &mut [epoll_event]) -> KResult<usize> {
        trace!("Epoll: poll_events called, out.len()={}", out.len());
        let mut count = 0;
        loop {
            let weak_interest = {
                let mut queue = self.inner.ready_queue.lock();
                queue.pop_front()
            };

            let Some(weak_interest) = weak_interest else {
                break;
            };

            if count >= out.len() {
                self.inner.ready_queue.lock().push_front(weak_interest);
                break;
            }

            let Some(interest) = weak_interest.upgrade() else {
                continue; // interest already removed
            };

            let Some(file) = interest.key.get_file() else {
                // file already closed remove interests
                self.inner.interests.lock().remove(&interest.key);
                interest.mark_not_in_queue();
                continue;
            };

            trace!(
                "Epoll: consuming ready interest for fd={}, events={:?}",
                interest.key.fd, interest.event.events
            );

            match interest.consume(file.as_ref()) {
                ConsumeResult::EventAndKeep(event) => {
                    out[count] = epoll_event {
                        events: event.events.bits(),
                        data: event.user_data,
                    };
                    count += 1;
                    self.inner
                        .ready_queue
                        .lock()
                        .push_back(Arc::downgrade(&interest));
                }
                ConsumeResult::EventAndRemove(event) => {
                    out[count] = epoll_event {
                        events: event.events.bits(),
                        data: event.user_data,
                    };
                    count += 1;
                    interest.mark_not_in_queue();
                    self.register_waker_only(&interest);
                }
                ConsumeResult::NoEvent => {
                    interest.mark_not_in_queue();
                    self.register_waker_only(&interest);
                }
            }
        }

        if count == 0 {
            Err(KError::WouldBlock)
        } else {
            Ok(count)
        }
    }
}

impl FileLike for Epoll {
    fn path(&self) -> Cow<'_, str> {
        "anon_inode:[eventpoll]".into()
    }
}

impl Pollable for Epoll {
    fn poll(&self) -> IoEvents {
        if self.inner.ready_queue.lock().is_empty() {
            IoEvents::empty()
        } else {
            IoEvents::IN
        }
    }

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        if events.contains(IoEvents::IN) {
            self.inner.poll_ready.register(context.waker());
        }
    }
}

#[cfg(unittest)]
mod epoll_tests {
    use kpoll::IoEvents;
    use unittest::def_test;

    use super::*;

    /// Test basic Epoll creation
    #[def_test]
    fn test_epoll_creation() {
        let epoll = Epoll::new();
        // Check that it implements FileLike correctly
        assert_eq!(epoll.path(), "anon_inode:[eventpoll]");
    }

    /// Test EpollFlags bitflags
    #[def_test]
    fn test_epoll_flags() {
        assert_eq!(EpollFlags::EDGE_TRIGGER.bits(), EPOLLET);
        assert_eq!(EpollFlags::ONESHOT.bits(), EPOLLONESHOT);

        let mut flags = EpollFlags::empty();
        flags.insert(EpollFlags::EDGE_TRIGGER);
        assert!(flags.contains(EpollFlags::EDGE_TRIGGER));

        flags.insert(EpollFlags::ONESHOT);
        assert!(flags.contains(EpollFlags::EDGE_TRIGGER | EpollFlags::ONESHOT));
    }

    /// Test TriggerMode creation from flags
    #[def_test]
    fn test_trigger_mode_from_flags() {
        match TriggerMode::from_flags(EpollFlags::empty()) {
            TriggerMode::Level => {} // Correct
            _ => panic!("Expected Level trigger"),
        }

        match TriggerMode::from_flags(EpollFlags::EDGE_TRIGGER) {
            TriggerMode::Edge => {} // Correct
            _ => panic!("Expected Edge trigger"),
        }

        match TriggerMode::from_flags(EpollFlags::ONESHOT) {
            TriggerMode::OneShot { fired: false } => {} // Correct
            _ => panic!("Expected OneShot with fired=false"),
        }

        // Test combined flags (ONESHOT takes precedence?)
        match TriggerMode::from_flags(EpollFlags::EDGE_TRIGGER | EpollFlags::ONESHOT) {
            TriggerMode::OneShot { fired: false } => {} // ONESHOT should take precedence
            _ => panic!("Expected OneShot with fired=false"),
        }
    }

    /// Test TriggerMode should_notify logic
    #[def_test]
    fn test_trigger_mode_should_notify() {
        // Level trigger: always notify
        let (should_notify, new_mode) = TriggerMode::Level.should_notify();
        assert!(should_notify);
        match new_mode {
            TriggerMode::Level => {} // Correct
            _ => panic!("Should remain Level"),
        }

        // Edge trigger: notify
        let (should_notify, new_mode) = TriggerMode::Edge.should_notify();
        assert!(should_notify);
        match new_mode {
            TriggerMode::Edge => {} // Correct
            _ => panic!("Should remain Edge"),
        }

        // OneShot first time: notify and become fired
        let (should_notify, new_mode) = TriggerMode::OneShot { fired: false }.should_notify();
        assert!(should_notify);
        match new_mode {
            TriggerMode::OneShot { fired: true } => {} // Correct
            _ => panic!("Should become fired=true"),
        }

        // OneShot after fired: don't notify
        let (should_notify, new_mode) = TriggerMode::OneShot { fired: true }.should_notify();
        assert!(!should_notify);
        match new_mode {
            TriggerMode::OneShot { fired: true } => {} // Correct
            _ => panic!("Should remain fired=true"),
        }
    }

    /// Test TriggerMode is_enabled
    #[def_test]
    fn test_trigger_mode_is_enabled() {
        assert!(TriggerMode::Level.is_enabled());
        assert!(TriggerMode::Edge.is_enabled());
        assert!(TriggerMode::OneShot { fired: false }.is_enabled());
        assert!(!TriggerMode::OneShot { fired: true }.is_enabled());
    }

    /// Test EpollEvent creation
    #[def_test]
    fn test_epoll_event() {
        let event = EpollEvent {
            events: IoEvents::IN | IoEvents::OUT,
            user_data: 0x12345678,
        };

        assert!(event.events.contains(IoEvents::IN));
        assert!(event.events.contains(IoEvents::OUT));
        assert!(!event.events.contains(IoEvents::ERR));
        assert_eq!(event.user_data, 0x12345678);
    }

    /// Test poll_events with zero-length buffer
    #[def_test]
    fn test_poll_events_zero_buffer() {
        let epoll = Epoll::new();
        let mut events = [];

        // Zero buffer should return WouldBlock (no space for events)
        let result = epoll.poll_events(&mut events);
        assert_eq!(result, Err(KError::WouldBlock));
    }
}

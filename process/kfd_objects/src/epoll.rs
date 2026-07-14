// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Epoll instance and interest management.

use alloc::{
    collections::vec_deque::VecDeque,
    sync::{Arc, Weak},
    task::Wake,
    vec::Vec,
};
use core::{
    hash::{Hash, Hasher},
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    task::{Context, Waker},
};

use bitflags::bitflags;
use hashbrown::{HashMap, HashSet};
use kerrno::{KError, KResult};
use kpoll::{IoEvents, PollSet, Pollable};
use kspin::SpinNoPreempt;
use kvfs::{AnonInodeFs, FMode, FileOperations, OpenFlags, VfsFile, VfsInode};
use linux_raw_sys::general::{EPOLLET, EPOLLONESHOT, epoll_event};

/// A ready event returned by an [`Epoll`] instance.
pub struct EpollEvent {
    /// Interested I/O events.
    pub events: IoEvents,
    /// User data associated with the interest.
    pub user_data: u64,
}

bitflags! {
    /// Flags for entries in an `epoll` instance.
    #[derive(Debug, Clone, Copy, Default)]
    pub struct EpollFlags: u32 {
        const EDGE_TRIGGER = EPOLLET;
        const ONESHOT = EPOLLONESHOT;
    }
}

/// Interest trigger mode.
#[derive(Debug, Clone, Copy)]
enum TriggerMode {
    /// Level-triggered: until the condition is cleared.
    Level,
    /// Edge-triggered: only notify when the condition changes.
    Edge,
    /// One-shot: notify only once.
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

    fn should_notify(&self) -> (bool, Self) {
        match self {
            TriggerMode::Level => (true, *self),
            TriggerMode::Edge => (true, TriggerMode::Edge),
            TriggerMode::OneShot { fired } => {
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
    /// Return an event and keep the interest queued for another level-triggered poll.
    EventAndKeep(EpollEvent),
    /// Return an event and remove only this ready-queue entry after consumption.
    EventAndRemove(EpollEvent),
    /// Return no event and rearm the interest according to the supplied policy.
    NoEvent {
        queue_current_events: IoEvents,
        queue_registered_wake: bool,
        registered_events: IoEvents,
        post_register_poll: bool,
    },
}

fn match_ready_events(current: IoEvents, interested: IoEvents) -> IoEvents {
    (current & interested) | (current & IoEvents::ALWAYS_POLL)
}

fn register_events(interested: IoEvents) -> IoEvents {
    interested | IoEvents::ALWAYS_POLL
}

#[derive(Clone)]
struct EntryKey {
    fd: i32,
    file: Weak<VfsFile>,
}

impl EntryKey {
    fn new(fd: i32, file: &Arc<VfsFile>) -> Self {
        Self {
            fd,
            file: Arc::downgrade(file),
        }
    }

    #[inline]
    fn get_file(&self) -> Option<Arc<VfsFile>> {
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
    last_reported_events: AtomicUsize,
    waker_generation: AtomicUsize,
}

impl EpollInterest {
    fn new(key: EntryKey, event: EpollEvent, flags: EpollFlags) -> Self {
        Self {
            key,
            event,
            mode: SpinNoPreempt::new(TriggerMode::from_flags(flags)),
            in_ready_queue: AtomicBool::new(false),
            last_reported_events: AtomicUsize::new(IoEvents::empty().bits() as usize),
            waker_generation: AtomicUsize::new(0),
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

    fn consume(&self, file: &VfsFile) -> ConsumeResult {
        let current_events = file.poll();
        let matched = match_ready_events(current_events, self.event.events);
        if matched.is_empty() {
            return self.no_event_rearm_current_ready();
        }

        let mut mode = self.mode.lock();
        if matches!(*mode, TriggerMode::Edge) && !self.should_notify_edge(matched) {
            return self.no_event_wait_for_transition();
        }
        let (should_notify, new_mode) = mode.should_notify();
        *mode = new_mode;
        trace!(
            "consume fd: {} matches {:?} should notify: {} ",
            self.key.fd, matched, should_notify
        );

        if !should_notify {
            return self.no_event_rearm_current_ready();
        }

        let event = EpollEvent {
            events: matched,
            user_data: self.event.user_data,
        };

        match *mode {
            TriggerMode::Level => ConsumeResult::EventAndKeep(event),
            TriggerMode::Edge | TriggerMode::OneShot { .. } => ConsumeResult::EventAndRemove(event),
        }
    }

    fn next_waker_generation(&self) -> usize {
        self.waker_generation.fetch_add(1, Ordering::AcqRel) + 1
    }

    fn is_current_waker_generation(&self, generation: usize) -> bool {
        self.waker_generation.load(Ordering::Acquire) == generation
    }

    fn no_event_rearm_current_ready(&self) -> ConsumeResult {
        let registered_events = register_events(self.event.events);
        self.last_reported_events
            .store(IoEvents::empty().bits() as usize, Ordering::Release);
        ConsumeResult::NoEvent {
            queue_current_events: registered_events,
            queue_registered_wake: false,
            registered_events,
            post_register_poll: true,
        }
    }

    fn no_event_wait_for_transition(&self) -> ConsumeResult {
        let registered_events = register_events(self.event.events);
        ConsumeResult::NoEvent {
            queue_current_events: IoEvents::empty(),
            queue_registered_wake: true,
            registered_events,
            post_register_poll: false,
        }
    }

    fn should_notify_edge(&self, matched: IoEvents) -> bool {
        let edge_events = matched - IoEvents::ALWAYS_POLL;
        if matched.intersects(IoEvents::ALWAYS_POLL) {
            self.last_reported_events
                .store(edge_events.bits() as usize, Ordering::Release);
            return true;
        }

        let matched_bits = edge_events.bits() as usize;
        self.last_reported_events
            .swap(matched_bits, Ordering::AcqRel)
            != matched_bits
    }

    fn clear_reported_edge_events(&self) {
        self.last_reported_events
            .store(IoEvents::empty().bits() as usize, Ordering::Release);
    }
}

struct InterestWaker {
    epoll: Weak<EpollInner>,
    interest: Weak<EpollInterest>,
    defer_wake: AtomicBool,
    deferred_wake: AtomicBool,
    generation: usize,
}

impl Wake for InterestWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        if self.defer_wake.load(Ordering::Acquire) {
            self.deferred_wake.store(true, Ordering::Release);
            return;
        }

        if let Some(interest) = self.interest.upgrade() {
            if !interest.is_current_waker_generation(self.generation) {
                return;
            }
            interest.clear_reported_edge_events();
            self.queue_interest_with(interest);
        }
    }
}

impl InterestWaker {
    fn new(epoll: &Arc<EpollInner>, interest: &Arc<EpollInterest>) -> Arc<Self> {
        let generation = interest.next_waker_generation();
        Arc::new(Self {
            epoll: Arc::downgrade(epoll),
            interest: Arc::downgrade(interest),
            defer_wake: AtomicBool::new(true),
            deferred_wake: AtomicBool::new(false),
            generation,
        })
    }

    fn finish_register(
        &self,
        ready_events: IoEvents,
        queue_current_events: IoEvents,
        queue_registered_wake: bool,
    ) {
        self.defer_wake.store(false, Ordering::Release);
        let had_registered_wake = self.deferred_wake.swap(false, Ordering::AcqRel);
        if ready_events.intersects(queue_current_events)
            || (queue_registered_wake && had_registered_wake)
        {
            self.queue_interest();
        }
    }

    fn queue_interest(&self) {
        let Some(interest) = self.interest.upgrade() else {
            return;
        };
        if !interest.is_current_waker_generation(self.generation) {
            return;
        }
        self.queue_interest_with(interest);
    }

    fn queue_interest_with(&self, interest: Arc<EpollInterest>) {
        let Some(epoll) = self.epoll.upgrade() else {
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

/// An epoll instance.
#[derive(Default)]
pub struct Epoll {
    inner: Arc<EpollInner>,
}

impl Epoll {
    /// Creates a new epoll instance.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an anonymous-inode file for this epoll instance.
    pub fn new_file() -> KResult<Arc<VfsFile>> {
        AnonInodeFs::global().get_file(
            "[eventpoll]",
            Arc::new(EventpollFops),
            Arc::new(Self::new()),
            FMode::READ | FMode::WRITE | FMode::STREAM,
            OpenFlags::READ_WRITE,
        )
    }

    /// Returns the epoll instance attached to an epoll file.
    pub fn from_file(file: &VfsFile) -> KResult<Arc<Self>> {
        file.private_data_get::<Self>()
            .ok_or(KError::BadFileDescriptor)
    }

    fn register_waker_only(
        &self,
        interest: &Arc<EpollInterest>,
        queue_current_events: IoEvents,
        queue_registered_wake: bool,
        registered_events: IoEvents,
        post_register_poll: bool,
    ) {
        let Some(file) = interest.key.get_file() else {
            return;
        };
        if !interest.is_enabled() {
            return;
        }

        let interest_waker = InterestWaker::new(&self.inner, interest);
        let waker = Waker::from(interest_waker.clone());

        let mut context = Context::from_waker(&waker);
        file.register_poll(&mut context, registered_events);
        let current = if post_register_poll {
            match_ready_events(file.poll(), interest.event.events)
        } else {
            IoEvents::empty()
        };
        interest_waker.finish_register(current, queue_current_events, queue_registered_wake);
    }

    fn replace_ready_interest(
        &self,
        old_interest: &Arc<EpollInterest>,
        new_interest: &Arc<EpollInterest>,
    ) {
        let old_weak = Arc::downgrade(old_interest);
        let new_weak = Arc::downgrade(new_interest);
        let mut queue = self.inner.ready_queue.lock();
        let mut replaced = false;

        for queued_interest in queue.iter_mut() {
            if Weak::ptr_eq(queued_interest, &old_weak) {
                *queued_interest = new_weak.clone();
                replaced = true;
                break;
            }
        }

        if !replaced {
            queue.push_back(new_weak);
        }
        new_interest.in_ready_queue.store(true, Ordering::Release);
        drop(queue);
        self.inner.poll_ready.wake();
    }

    fn check_and_register_waker(&self, interest: &Arc<EpollInterest>) {
        let Some(file) = interest.key.get_file() else {
            return;
        };
        if !interest.is_enabled() {
            return;
        }

        let interest_waker = InterestWaker::new(&self.inner, interest);
        let waker = Waker::from(interest_waker.clone());

        let current = match_ready_events(file.poll(), interest.event.events);
        if !current.is_empty() {
            waker.wake_by_ref();
            interest_waker.finish_register(current, register_events(interest.event.events), false);
        } else {
            let mut context = Context::from_waker(&waker);
            file.register_poll(&mut context, register_events(interest.event.events));

            let current = match_ready_events(file.poll(), interest.event.events);
            interest_waker.finish_register(current, register_events(interest.event.events), false);
        }
    }

    /// Adds a file descriptor interest to the epoll instance.
    pub fn add(
        &self,
        fd: i32,
        file: Arc<VfsFile>,
        event: EpollEvent,
        flags: EpollFlags,
    ) -> KResult<()> {
        let key = EntryKey::new(fd, &file);
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
    pub fn modify(
        &self,
        fd: i32,
        file: Arc<VfsFile>,
        event: EpollEvent,
        flags: EpollFlags,
    ) -> KResult<()> {
        let key = EntryKey::new(fd, &file);
        let interest = Arc::new(EpollInterest::new(key.clone(), event, flags));

        let mut guard = self.inner.interests.lock();
        let old = guard.get(&key).cloned().ok_or(KError::NotFound)?;
        guard.insert(key.clone(), Arc::clone(&interest));
        drop(guard);

        if old.is_in_queue() {
            self.replace_ready_interest(&old, &interest);
            let registered_events = register_events(interest.event.events);
            self.register_waker_only(&interest, registered_events, true, registered_events, true);
        } else {
            self.check_and_register_waker(&interest);
        }

        trace!(
            "Epoll: modify fd={}, events={:?}",
            fd, interest.event.events
        );
        Ok(())
    }

    /// Removes an existing interest for the given file descriptor.
    pub fn delete(&self, fd: i32, file: Arc<VfsFile>) -> KResult<()> {
        let key = EntryKey::new(fd, &file);
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
        let mut deferred_keep = Vec::new();
        // Bound this call to the entries that were ready when it started.
        // Rearming an interest while consuming it can synchronously enqueue the
        // same interest again through `finish_register()`. Leaving those new
        // entries for the next call prevents a persistently-ready fd from
        // keeping one `epoll_wait` call alive forever.
        let (ready_count, mut first_ready_interest) = {
            let mut queue = self.inner.ready_queue.lock();
            let ready_count = queue.len();
            (ready_count, queue.pop_front())
        };
        let mut emitted = HashSet::with_capacity(ready_count);
        for index in 0..ready_count {
            let weak_interest = if index == 0 {
                first_ready_interest.take()
            } else {
                let mut queue = self.inner.ready_queue.lock();
                queue.pop_front()
            };

            let Some(weak_interest) = weak_interest else {
                break;
            };
            let Some(interest) = weak_interest.upgrade() else {
                continue;
            };
            let Some(file) = interest.key.get_file() else {
                self.inner.interests.lock().remove(&interest.key);
                interest.mark_not_in_queue();
                continue;
            };
            if emitted.contains(&interest.key) {
                continue;
            }
            emitted.insert(interest.key.clone());
            if count >= out.len() {
                self.inner.ready_queue.lock().push_front(weak_interest);
                break;
            }

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
                    deferred_keep.push(Arc::downgrade(&interest));
                }
                ConsumeResult::EventAndRemove(event) => {
                    out[count] = epoll_event {
                        events: event.events.bits(),
                        data: event.user_data,
                    };
                    count += 1;
                    interest.mark_not_in_queue();
                    let registered_events = register_events(interest.event.events);
                    self.register_waker_only(
                        &interest,
                        IoEvents::empty(),
                        false,
                        registered_events,
                        false,
                    );
                }
                ConsumeResult::NoEvent {
                    queue_current_events,
                    queue_registered_wake,
                    registered_events,
                    post_register_poll,
                } => {
                    interest.mark_not_in_queue();
                    self.register_waker_only(
                        &interest,
                        queue_current_events,
                        queue_registered_wake,
                        registered_events,
                        post_register_poll,
                    );
                }
            }
        }

        if !deferred_keep.is_empty() {
            let mut queue = self.inner.ready_queue.lock();
            for interest in deferred_keep {
                queue.push_back(interest);
            }
        }

        if count == 0 {
            Err(KError::WouldBlock)
        } else {
            Ok(count)
        }
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

struct EventpollFops;

impl FileOperations for EventpollFops {
    fn release(&self, _inode: &VfsInode, _file: &VfsFile) -> KResult<()> {
        Ok(())
    }

    fn poll(&self, file: &VfsFile) -> IoEvents {
        Epoll::from_file(file).map_or(IoEvents::ERR, |epoll| epoll.poll())
    }

    fn register_poll(&self, file: &VfsFile, context: &mut Context<'_>, events: IoEvents) {
        if let Ok(epoll) = Epoll::from_file(file) {
            epoll.register(context, events);
        }
    }
}

#[cfg(unittest)]
mod epoll_tests {
    use alloc::sync::Arc;
    use core::{
        ptr,
        sync::atomic::{AtomicUsize, Ordering},
        task::Context,
    };

    use kerrno::KError;
    use kpoll::{IoEvents, PollSet, Pollable};
    use kvfs::{AnonInodeFs, FMode, FileOperations, OpenFlags, VfsFile};
    use linux_raw_sys::general::{EPOLLET, EPOLLONESHOT, epoll_event};
    use unittest::def_test;

    use super::{
        EntryKey, Epoll, EpollEvent, EpollFlags, EpollInterest, TriggerMode, match_ready_events,
        register_events,
    };

    fn assert_epoll_event(event: &epoll_event, events: u32, data: u64) {
        // SAFETY: `event` is a valid initialized `epoll_event`. The Linux ABI
        // type is packed, so field reads must be explicitly unaligned.
        let actual_events = unsafe { ptr::addr_of!(event.events).read_unaligned() };
        // SAFETY: `event` is a valid initialized `epoll_event`. The Linux ABI
        // type is packed, so field reads must be explicitly unaligned.
        let actual_data = unsafe { ptr::addr_of!(event.data).read_unaligned() };

        assert_eq!(actual_events, events);
        assert_eq!(actual_data, data);
    }

    enum TestFileKind {
        Ready,
        Rewake,
        EmptyRewake,
        Hup,
        CountedOut,
        PollSetBacked,
    }

    struct TestFile {
        kind: TestFileKind,
        poll_count: AtomicUsize,
        poll_set: PollSet,
    }

    impl TestFile {
        fn new(kind: TestFileKind) -> Self {
            Self {
                kind,
                poll_count: AtomicUsize::new(0),
                poll_set: PollSet::new(),
            }
        }

        fn poll_count(&self) -> usize {
            self.poll_count.load(Ordering::Acquire)
        }
    }

    impl Pollable for TestFile {
        fn poll(&self) -> IoEvents {
            match self.kind {
                TestFileKind::Ready | TestFileKind::Rewake => IoEvents::IN,
                TestFileKind::EmptyRewake | TestFileKind::PollSetBacked => IoEvents::empty(),
                TestFileKind::Hup => IoEvents::HUP,
                TestFileKind::CountedOut => {
                    self.poll_count.fetch_add(1, Ordering::AcqRel);
                    IoEvents::OUT
                }
            }
        }

        fn register(&self, context: &mut Context<'_>, _events: IoEvents) {
            match self.kind {
                TestFileKind::Rewake | TestFileKind::EmptyRewake => {
                    context.waker().wake_by_ref();
                }
                TestFileKind::PollSetBacked => {
                    self.poll_set.register(context.waker());
                }
                _ => {}
            }
        }
    }

    struct TestFileFops;

    impl TestFileFops {
        fn state(file: &VfsFile) -> Arc<TestFile> {
            file.private_data_get::<TestFile>()
                .expect("test file private data is installed")
        }
    }

    impl FileOperations for TestFileFops {
        fn poll(&self, file: &VfsFile) -> IoEvents {
            Self::state(file).poll()
        }

        fn register_poll(&self, file: &VfsFile, context: &mut Context<'_>, events: IoEvents) {
            Self::state(file).register(context, events);
        }
    }

    fn test_file(kind: TestFileKind) -> Arc<VfsFile> {
        AnonInodeFs::global()
            .get_file(
                "[epoll-test]",
                Arc::new(TestFileFops),
                Arc::new(TestFile::new(kind)),
                FMode::READ | FMode::WRITE | FMode::STREAM,
                OpenFlags::empty(),
            )
            .expect("test file opens")
    }

    fn test_file_state(file: &VfsFile) -> Arc<TestFile> {
        TestFileFops::state(file)
    }

    #[def_test]
    fn test_epoll_creation() {
        let file = Epoll::new_file().expect("epoll file opens");
        assert_eq!(
            file.path().absolute_path().unwrap().as_str(),
            "anon_inode:[eventpoll]"
        );
    }

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

    #[def_test]
    fn test_trigger_mode_from_flags() {
        match TriggerMode::from_flags(EpollFlags::empty()) {
            TriggerMode::Level => {}
            _ => panic!("Expected Level trigger"),
        }

        match TriggerMode::from_flags(EpollFlags::EDGE_TRIGGER) {
            TriggerMode::Edge => {}
            _ => panic!("Expected Edge trigger"),
        }

        match TriggerMode::from_flags(EpollFlags::ONESHOT) {
            TriggerMode::OneShot { fired: false } => {}
            _ => panic!("Expected OneShot with fired=false"),
        }

        match TriggerMode::from_flags(EpollFlags::EDGE_TRIGGER | EpollFlags::ONESHOT) {
            TriggerMode::OneShot { fired: false } => {}
            _ => panic!("Expected OneShot with fired=false"),
        }
    }

    #[def_test]
    fn test_trigger_mode_should_notify() {
        let (should_notify, new_mode) = TriggerMode::Level.should_notify();
        assert!(should_notify);
        assert!(matches!(new_mode, TriggerMode::Level));

        let (should_notify, new_mode) = TriggerMode::Edge.should_notify();
        assert!(should_notify);
        assert!(matches!(new_mode, TriggerMode::Edge));

        let (should_notify, new_mode) = TriggerMode::OneShot { fired: false }.should_notify();
        assert!(should_notify);
        assert!(matches!(new_mode, TriggerMode::OneShot { fired: true }));

        let (should_notify, new_mode) = TriggerMode::OneShot { fired: true }.should_notify();
        assert!(!should_notify);
        assert!(matches!(new_mode, TriggerMode::OneShot { fired: true }));
    }

    #[def_test]
    fn test_trigger_mode_is_enabled() {
        assert!(TriggerMode::Level.is_enabled());
        assert!(TriggerMode::Edge.is_enabled());
        assert!(TriggerMode::OneShot { fired: false }.is_enabled());
        assert!(!TriggerMode::OneShot { fired: true }.is_enabled());
    }

    #[def_test]
    fn test_epoll_event() {
        let event = EpollEvent {
            events: IoEvents::IN | IoEvents::OUT,
            user_data: 123,
        };
        assert_eq!(event.events.bits(), (IoEvents::IN | IoEvents::OUT).bits());
        assert_eq!(event.user_data, 123);
    }

    #[def_test]
    fn test_epoll_always_poll_events() {
        let interested = IoEvents::IN;
        assert_eq!(
            register_events(interested).bits(),
            (IoEvents::IN | IoEvents::ALWAYS_POLL).bits()
        );
        assert_eq!(
            match_ready_events(IoEvents::HUP | IoEvents::OUT, interested).bits(),
            IoEvents::HUP.bits()
        );
    }

    #[def_test]
    fn test_epoll_poll_no_events() {
        let epoll = Epoll::new();
        let events = epoll.poll();
        assert!(events.is_empty());
    }

    #[def_test]
    fn test_epoll_poll_events_empty() {
        let epoll = Epoll::new();
        let mut out = [epoll_event { events: 0, data: 0 }; 4];
        assert_eq!(epoll.poll_events(&mut out), Err(KError::WouldBlock));
    }

    #[def_test]
    fn test_epoll_poll_events_deduplicates_ready_interest() {
        let epoll = Epoll::new();
        let file = test_file(TestFileKind::Ready);
        let key = EntryKey::new(3, &file);
        let interest = Arc::new(EpollInterest::new(
            key,
            EpollEvent {
                events: IoEvents::IN,
                user_data: 7,
            },
            EpollFlags::empty(),
        ));
        interest.in_ready_queue.store(true, Ordering::Release);

        let weak = Arc::downgrade(&interest);
        {
            let mut queue = epoll.inner.ready_queue.lock();
            queue.push_back(weak.clone());
            queue.push_back(weak);
        }

        let mut out = [epoll_event { events: 0, data: 0 }; 1];
        assert_eq!(epoll.poll_events(&mut out).unwrap(), 1);
        assert_epoll_event(&out[0], IoEvents::IN.bits(), 7);
        assert_eq!(epoll.inner.ready_queue.lock().len(), 1);
    }

    #[def_test]
    fn test_epoll_poll_events_ignores_edge_synchronous_rewake_after_rearm() {
        let epoll = Epoll::new();
        let file = test_file(TestFileKind::Rewake);

        epoll
            .add(
                3,
                file.clone(),
                EpollEvent {
                    events: IoEvents::IN,
                    user_data: 7,
                },
                EpollFlags::EDGE_TRIGGER,
            )
            .unwrap();

        let mut out = [epoll_event { events: 0, data: 0 }; 1];
        assert_eq!(epoll.poll_events(&mut out).unwrap(), 1);
        assert_epoll_event(&out[0], IoEvents::IN.bits(), 7);
        assert_eq!(epoll.poll().bits(), IoEvents::empty().bits());
    }

    #[def_test]
    fn test_epoll_poll_events_does_not_requeue_edge_level_ready_file() {
        let epoll = Epoll::new();
        let file = test_file(TestFileKind::Ready);
        let key = EntryKey::new(3, &file);
        let interest = Arc::new(EpollInterest::new(
            key,
            EpollEvent {
                events: IoEvents::IN,
                user_data: 7,
            },
            EpollFlags::EDGE_TRIGGER,
        ));
        interest.in_ready_queue.store(true, Ordering::Release);

        epoll
            .inner
            .ready_queue
            .lock()
            .push_back(Arc::downgrade(&interest));

        let mut out = [epoll_event { events: 0, data: 0 }; 1];
        assert_eq!(epoll.poll_events(&mut out).unwrap(), 1);
        assert_epoll_event(&out[0], IoEvents::IN.bits(), 7);
        assert_eq!(epoll.poll().bits(), IoEvents::empty().bits());
    }

    #[def_test]
    fn test_epoll_poll_events_drops_duplicate_edge_synchronous_rewake() {
        let epoll = Epoll::new();
        let file = test_file(TestFileKind::Rewake);
        let key = EntryKey::new(3, &file);
        let interest = Arc::new(EpollInterest::new(
            key,
            EpollEvent {
                events: IoEvents::IN,
                user_data: 7,
            },
            EpollFlags::EDGE_TRIGGER,
        ));
        interest.in_ready_queue.store(true, Ordering::Release);

        let weak = Arc::downgrade(&interest);
        {
            let mut queue = epoll.inner.ready_queue.lock();
            queue.push_back(weak.clone());
            queue.push_back(weak);
        }

        let mut out = [epoll_event { events: 0, data: 0 }; 1];
        assert_eq!(epoll.poll_events(&mut out).unwrap(), 1);
        assert_epoll_event(&out[0], IoEvents::IN.bits(), 7);
        assert_eq!(epoll.poll().bits(), IoEvents::empty().bits());
    }

    #[def_test]
    fn test_epoll_poll_events_drops_empty_synchronous_rewake() {
        let epoll = Epoll::new();
        let file = test_file(TestFileKind::EmptyRewake);
        let key = EntryKey::new(3, &file);
        let interest = Arc::new(EpollInterest::new(
            key,
            EpollEvent {
                events: IoEvents::IN,
                user_data: 7,
            },
            EpollFlags::EDGE_TRIGGER,
        ));
        interest.in_ready_queue.store(true, Ordering::Release);

        epoll
            .inner
            .ready_queue
            .lock()
            .push_back(Arc::downgrade(&interest));

        let mut out = [epoll_event { events: 0, data: 0 }; 1];
        assert_eq!(epoll.poll_events(&mut out), Err(KError::WouldBlock));
        assert_eq!(epoll.poll().bits(), IoEvents::empty().bits());
    }

    #[def_test]
    fn test_epoll_edge_does_not_suppress_always_poll_events() {
        let epoll = Epoll::new();
        let file = test_file(TestFileKind::Hup);
        let key = EntryKey::new(3, &file);
        let interest = Arc::new(EpollInterest::new(
            key,
            EpollEvent {
                events: IoEvents::IN,
                user_data: 7,
            },
            EpollFlags::EDGE_TRIGGER,
        ));
        interest.in_ready_queue.store(true, Ordering::Release);
        epoll
            .inner
            .ready_queue
            .lock()
            .push_back(Arc::downgrade(&interest));

        let mut out = [epoll_event { events: 0, data: 0 }; 1];
        assert_eq!(epoll.poll_events(&mut out).unwrap(), 1);
        assert_epoll_event(&out[0], IoEvents::HUP.bits(), 7);

        assert!(interest.try_mark_in_queue());
        epoll
            .inner
            .ready_queue
            .lock()
            .push_back(Arc::downgrade(&interest));

        assert_eq!(epoll.poll_events(&mut out).unwrap(), 1);
        assert_epoll_event(&out[0], IoEvents::HUP.bits(), 7);
    }

    #[def_test]
    fn test_epoll_edge_rearm_skips_post_register_poll() {
        let epoll = Epoll::new();
        let file = test_file(TestFileKind::CountedOut);
        let state = test_file_state(&file);
        let key = EntryKey::new(3, &file);
        let interest = Arc::new(EpollInterest::new(
            key,
            EpollEvent {
                events: IoEvents::OUT,
                user_data: 7,
            },
            EpollFlags::EDGE_TRIGGER,
        ));
        interest.in_ready_queue.store(true, Ordering::Release);

        epoll
            .inner
            .ready_queue
            .lock()
            .push_back(Arc::downgrade(&interest));

        let mut out = [epoll_event { events: 0, data: 0 }; 1];
        assert_eq!(epoll.poll_events(&mut out).unwrap(), 1);
        assert_epoll_event(&out[0], IoEvents::OUT.bits(), 7);
        assert_eq!(state.poll_count(), 1);
    }

    #[def_test]
    fn test_epoll_rearm_limits_stale_registered_wakers() {
        let epoll = Epoll::new();
        let file = test_file(TestFileKind::PollSetBacked);
        let key = EntryKey::new(3, &file);
        let interest = Arc::new(EpollInterest::new(
            key,
            EpollEvent {
                events: IoEvents::IN,
                user_data: 7,
            },
            EpollFlags::EDGE_TRIGGER,
        ));

        for _ in 0..70 {
            epoll.register_waker_only(
                &interest,
                IoEvents::empty(),
                false,
                register_events(interest.event.events),
                false,
            );
        }

        assert!(epoll.inner.ready_queue.lock().len() <= 1);

        let mut out = [epoll_event { events: 0, data: 0 }; 1];
        assert_eq!(epoll.poll_events(&mut out), Err(KError::WouldBlock));
        assert_eq!(epoll.poll().bits(), IoEvents::empty().bits());
    }

    #[def_test]
    fn test_epoll_delete_nonexistent() {
        let epoll = Epoll::new();
        let dummy = Epoll::new_file().expect("epoll file opens");
        assert!(epoll.delete(999, dummy).is_err());
    }

    #[def_test]
    fn test_epoll_modify_nonexistent() {
        let epoll = Epoll::new();
        let dummy = Epoll::new_file().expect("epoll file opens");
        let event = EpollEvent {
            events: IoEvents::IN,
            user_data: 42,
        };
        assert!(
            epoll
                .modify(999, dummy, event, EpollFlags::empty())
                .is_err()
        );
    }

    #[def_test]
    fn test_epoll_default() {
        let epoll = Epoll::default();
        assert!(epoll.poll().is_empty());
    }
}

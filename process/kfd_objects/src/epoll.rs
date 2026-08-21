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
    mem,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    task::{Context, Waker},
};

use anon_inodefs::AnonInodeFs;
use bitflags::bitflags;
use hashbrown::{HashMap, HashSet};
use kcred::Cred;
use kerrno::{KError, KResult};
use kpoll::{IoEvents, PollContext, PollRegisterError, PollRegistrations, PollSet, Pollable};
use kspin::SpinNoPreempt;
use ksync::Mutex;
use kvfs::{FMode, FileOperations, OpenFlags, VfsFile, VfsInode};
use linux_raw_sys::general::{EPOLLET, EPOLLONESHOT, epoll_event};

/// A ready event returned by an [`Epoll`] instance.
#[derive(Clone, Copy)]
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

#[derive(Clone, Copy)]
struct EpollInterestConfig {
    event: EpollEvent,
    mode: TriggerMode,
}

impl EpollInterestConfig {
    fn new(event: EpollEvent, flags: EpollFlags) -> Self {
        Self {
            event,
            mode: TriggerMode::from_flags(flags),
        }
    }
}

struct EpollInterestSnapshot {
    config: EpollInterestConfig,
    last_reported_events: usize,
}

fn match_ready_events(current: IoEvents, interested: IoEvents) -> IoEvents {
    (current & interested) | (current & IoEvents::ALWAYS_POLL)
}

fn register_events(interested: IoEvents) -> IoEvents {
    interested | IoEvents::ALWAYS_POLL
}

fn map_register_error(error: PollRegisterError) -> KError {
    match error {
        PollRegisterError::NoMemory | PollRegisterError::IdExhausted => KError::NoMemory,
        PollRegisterError::InvalidState => KError::InvalidInput,
    }
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
    config: SpinNoPreempt<EpollInterestConfig>,
    // Sleepable: held only while swapping the owner, not across file poll hooks.
    // Old `PollRegistrations` must be dropped outside this lock so source
    // `SpinNoIrq` unregister work does not nest under the Mutex.
    registrations: Mutex<PollRegistrations>,
    in_ready_queue: AtomicBool,
    last_reported_events: AtomicUsize,
    waker_generation: AtomicUsize,
}

impl EpollInterest {
    fn new(key: EntryKey, event: EpollEvent, flags: EpollFlags) -> Self {
        Self {
            key,
            config: SpinNoPreempt::new(EpollInterestConfig::new(event, flags)),
            registrations: Mutex::new(PollRegistrations::new()),
            in_ready_queue: AtomicBool::new(false),
            last_reported_events: AtomicUsize::new(IoEvents::empty().bits() as usize),
            waker_generation: AtomicUsize::new(0),
        }
    }

    #[inline]
    fn is_enabled(&self) -> bool {
        self.config.lock().mode.is_enabled()
    }

    #[inline]
    fn event_snapshot(&self) -> EpollEvent {
        self.config.lock().event
    }

    #[inline]
    fn interested_events(&self) -> IoEvents {
        self.event_snapshot().events
    }

    #[inline]
    fn registered_events(&self) -> IoEvents {
        register_events(self.interested_events())
    }

    fn modify_config(&self, event: EpollEvent, flags: EpollFlags) -> EpollInterestSnapshot {
        let last_reported_events = self
            .last_reported_events
            .swap(IoEvents::empty().bits() as usize, Ordering::AcqRel);
        let config = {
            let mut config = self.config.lock();
            let previous = *config;
            *config = EpollInterestConfig::new(event, flags);
            previous
        };
        EpollInterestSnapshot {
            config,
            last_reported_events,
        }
    }

    fn restore_config(&self, snapshot: EpollInterestSnapshot) {
        *self.config.lock() = snapshot.config;
        self.last_reported_events
            .store(snapshot.last_reported_events, Ordering::Release);
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

    /// Takes ownership of any live source registrations, releasing the Mutex
    /// before the returned value is dropped (and sources are unregistered).
    fn take_registrations(&self) -> PollRegistrations {
        let mut owned = self.registrations.lock();
        mem::take(&mut *owned)
    }

    /// Installs `registrations` and returns the previous owner so it can be
    /// dropped after the Mutex is released.
    fn replace_registrations(&self, registrations: PollRegistrations) -> PollRegistrations {
        let mut owned = self.registrations.lock();
        mem::replace(&mut *owned, registrations)
    }

    fn consume(&self, file: &VfsFile) -> ConsumeResult {
        let current_events = file.poll();
        let mut config = self.config.lock();
        let interested_events = config.event.events;
        let matched = match_ready_events(current_events, interested_events);
        if matched.is_empty() {
            return self.no_event_rearm_current_ready(interested_events);
        }

        if matches!(config.mode, TriggerMode::Edge) && !self.should_notify_edge(matched) {
            return self.no_event_wait_for_transition(interested_events);
        }
        let (should_notify, new_mode) = config.mode.should_notify();
        config.mode = new_mode;
        trace!(
            "consume fd: {} matches {:?} should notify: {} ",
            self.key.fd, matched, should_notify
        );

        if !should_notify {
            return self.no_event_rearm_current_ready(interested_events);
        }

        let event = EpollEvent {
            events: matched,
            user_data: config.event.user_data,
        };

        match config.mode {
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

    fn no_event_rearm_current_ready(&self, interested_events: IoEvents) -> ConsumeResult {
        let registered_events = register_events(interested_events);
        self.last_reported_events
            .store(IoEvents::empty().bits() as usize, Ordering::Release);
        ConsumeResult::NoEvent {
            queue_current_events: registered_events,
            queue_registered_wake: false,
            registered_events,
            post_register_poll: true,
        }
    }

    fn no_event_wait_for_transition(&self, interested_events: IoEvents) -> ConsumeResult {
        let registered_events = register_events(interested_events);
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
    ) -> bool {
        self.defer_wake.store(false, Ordering::Release);
        let had_registered_wake = self.deferred_wake.swap(false, Ordering::AcqRel);
        let should_queue = ready_events.intersects(queue_current_events)
            || (queue_registered_wake && had_registered_wake);
        if should_queue {
            self.queue_interest();
            // Queued: any one-shot drain is accounted for by the ready-queue entry.
            return false;
        }
        // Deferred wake was ignored (e.g. ET rearm). One-shot `PollSet::wake`
        // may already have detached the waiter; caller should restore once.
        had_registered_wake
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
                interest.key.fd,
                interest.interested_events()
            );
            epoll.poll_ready.wake();
        }
    }
}

struct EpollInner {
    // Serializes epoll_ctl-style updates that mutate the interest table and
    // per-interest registrations. Ready consumption keeps using finer locks.
    ctl_lock: Mutex<()>,
    interests: SpinNoPreempt<HashMap<EntryKey, Arc<EpollInterest>>>,
    ready_queue: SpinNoPreempt<VecDeque<Weak<EpollInterest>>>,
    poll_ready: PollSet,
}

impl Default for EpollInner {
    fn default() -> Self {
        Self {
            ctl_lock: Mutex::new(()),
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

    /// Creates an epoll anonymous-inode file and captures `cred` as its open credential.
    pub fn new_file(cred: Arc<Cred>) -> KResult<Arc<VfsFile>> {
        AnonInodeFs::global().get_file(
            "[eventpoll]",
            Arc::new(EventpollFops),
            Arc::new(Self::new()),
            FMode::READ | FMode::WRITE | FMode::STREAM,
            OpenFlags::READ_WRITE,
            cred,
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
    ) -> KResult<()> {
        self.register_waker_only_inner(
            interest,
            queue_current_events,
            queue_registered_wake,
            registered_events,
            post_register_poll,
            true,
        )
    }

    fn register_waker_only_inner(
        &self,
        interest: &Arc<EpollInterest>,
        queue_current_events: IoEvents,
        queue_registered_wake: bool,
        registered_events: IoEvents,
        post_register_poll: bool,
        allow_restore: bool,
    ) -> KResult<()> {
        let Some(file) = interest.key.get_file() else {
            drop(interest.take_registrations());
            return Ok(());
        };
        if !interest.is_enabled() {
            drop(interest.take_registrations());
            return Ok(());
        }

        // Install the new registration before dropping the old one so sources
        // never observe a zero-waiter gap. Generation bumps first so stale
        // InterestWakers from the previous round cannot requeue.
        let interest_waker = InterestWaker::new(&self.inner, interest);
        let waker = Waker::from(interest_waker.clone());
        let context = Context::from_waker(&waker);

        let mut registrations = PollRegistrations::new();
        {
            let mut poll_context = registrations.context(&context);
            file.register_poll(&mut poll_context, registered_events)
                .map_err(map_register_error)?;
        }
        // Drop the previous owner outside the Mutex to avoid nesting source
        // SpinNoIrq unregister under interest.registrations.
        drop(interest.replace_registrations(registrations));

        let current = if post_register_poll {
            match_ready_events(file.poll(), interest.interested_events())
        } else {
            IoEvents::empty()
        };
        let needs_restore =
            interest_waker.finish_register(current, queue_current_events, queue_registered_wake);
        // One-shot sources detach the waiter when they wake during defer. ET
        // rearm intentionally ignores that wake as a readiness edge, but must
        // restore a live registration once. A second deferred wake (test
        // files that wake on every register) stops here to avoid a loop.
        if needs_restore && allow_restore {
            return self.register_waker_only_inner(
                interest,
                queue_current_events,
                queue_registered_wake,
                registered_events,
                post_register_poll,
                false,
            );
        }
        Ok(())
    }

    fn check_and_register_waker(&self, interest: &Arc<EpollInterest>) -> KResult<()> {
        let Some(file) = interest.key.get_file() else {
            drop(interest.take_registrations());
            return Ok(());
        };
        if !interest.is_enabled() {
            drop(interest.take_registrations());
            return Ok(());
        }

        let interest_waker = InterestWaker::new(&self.inner, interest);
        let waker = Waker::from(interest_waker.clone());

        let registered_events = interest.registered_events();
        let current = match_ready_events(file.poll(), interest.interested_events());
        if !current.is_empty() {
            // Ready now: drop any prior registrations and do not leave a waiter.
            drop(interest.take_registrations());
            waker.wake_by_ref();
            let _ = interest_waker.finish_register(current, registered_events, false);
        } else {
            let context = Context::from_waker(&waker);
            let mut registrations = PollRegistrations::new();
            {
                let mut poll_context = registrations.context(&context);
                file.register_poll(&mut poll_context, registered_events)
                    .map_err(map_register_error)?;
            }
            drop(interest.replace_registrations(registrations));

            let current = match_ready_events(file.poll(), interest.interested_events());
            let needs_restore = interest_waker.finish_register(current, registered_events, false);
            if needs_restore {
                // One-shot drained during defer and we did not queue; restore.
                return self.register_waker_only(
                    interest,
                    registered_events,
                    false,
                    registered_events,
                    true,
                );
            }
        }
        Ok(())
    }

    /// Adds a file descriptor interest to the epoll instance.
    pub fn add(
        &self,
        fd: i32,
        file: Arc<VfsFile>,
        event: EpollEvent,
        flags: EpollFlags,
    ) -> KResult<()> {
        let _ctl_guard = self.inner.ctl_lock.lock();
        let key = EntryKey::new(fd, &file);
        let interest = Arc::new(EpollInterest::new(key.clone(), event, flags));
        let mut guard = self.inner.interests.lock();
        if guard.contains_key(&key) {
            return Err(KError::AlreadyExists);
        }
        guard.insert(key.clone(), Arc::clone(&interest));
        drop(guard);
        trace!(
            "Epoll add fd: {} interest {:?} ",
            fd,
            interest.interested_events()
        );
        if let Err(err) = self.check_and_register_waker(&interest) {
            let removed = {
                let mut guard = self.inner.interests.lock();
                if guard
                    .get(&key)
                    .is_some_and(|current| Arc::ptr_eq(current, &interest))
                {
                    guard.remove(&key)
                } else {
                    None
                }
            };
            drop(removed);
            return Err(err);
        }
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
        let _ctl_guard = self.inner.ctl_lock.lock();
        let key = EntryKey::new(fd, &file);
        let interest = self
            .inner
            .interests
            .lock()
            .get(&key)
            .cloned()
            .ok_or(KError::NotFound)?;
        let snapshot = interest.modify_config(event, flags);
        let registered_events = register_events(event.events);
        let was_in_queue = interest.is_in_queue();

        let registration_result = if was_in_queue {
            self.register_waker_only(&interest, registered_events, true, registered_events, true)
        } else {
            self.check_and_register_waker(&interest)
        };
        if let Err(err) = registration_result {
            interest.restore_config(snapshot);
            return Err(err);
        }
        if interest.is_in_queue() {
            self.inner.poll_ready.wake();
        }

        trace!(
            "Epoll: modify fd={}, events={:?}",
            fd,
            interest.interested_events()
        );
        Ok(())
    }

    /// Removes an existing interest for the given file descriptor.
    pub fn delete(&self, fd: i32, file: Arc<VfsFile>) -> KResult<()> {
        let _ctl_guard = self.inner.ctl_lock.lock();
        let key = EntryKey::new(fd, &file);
        let removed = self
            .inner
            .interests
            .lock()
            .remove(&key)
            .ok_or(KError::NotFound)?;
        drop(removed);
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
                let removed = self.inner.interests.lock().remove(&interest.key);
                drop(removed);
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
                interest.key.fd,
                interest.interested_events()
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
                    let registered_events = interest.registered_events();
                    if let Err(err) = self.register_waker_only(
                        &interest,
                        IoEvents::empty(),
                        false,
                        registered_events,
                        false,
                    ) {
                        // Event already copied out; keep partial results and
                        // requeue so a later poll can retry rearm.
                        return Self::finish_poll_events(
                            &self.inner,
                            count,
                            deferred_keep,
                            Some(Arc::downgrade(&interest)),
                            Some(err),
                        );
                    }
                }
                ConsumeResult::NoEvent {
                    queue_current_events,
                    queue_registered_wake,
                    registered_events,
                    post_register_poll,
                } => {
                    interest.mark_not_in_queue();
                    if let Err(err) = self.register_waker_only(
                        &interest,
                        queue_current_events,
                        queue_registered_wake,
                        registered_events,
                        post_register_poll,
                    ) {
                        return Self::finish_poll_events(
                            &self.inner,
                            count,
                            deferred_keep,
                            Some(Arc::downgrade(&interest)),
                            Some(err),
                        );
                    }
                }
            }
        }

        Self::finish_poll_events(&self.inner, count, deferred_keep, None, None)
    }

    fn finish_poll_events(
        inner: &EpollInner,
        count: usize,
        deferred_keep: Vec<Weak<EpollInterest>>,
        failed_rearm: Option<Weak<EpollInterest>>,
        rearm_error: Option<KError>,
    ) -> KResult<usize> {
        {
            let mut queue = inner.ready_queue.lock();
            for interest in deferred_keep {
                queue.push_back(interest);
            }
            if let Some(failed) = failed_rearm
                && let Some(interest) = failed.upgrade()
                && interest.try_mark_in_queue()
            {
                queue.push_back(failed);
            }
        }

        match rearm_error {
            Some(err) if count == 0 => Err(err),
            _ if count == 0 => Err(KError::WouldBlock),
            _ => Ok(count),
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

    fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if events.contains(IoEvents::IN) {
            context.register(&self.inner.poll_ready)?;
        }
        Ok(())
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

    fn register_poll(
        &self,
        file: &VfsFile,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if let Ok(epoll) = Epoll::from_file(file) {
            epoll.register(context, events)?;
        }
        Ok(())
    }
}

#[cfg(unittest)]
mod epoll_tests {
    use alloc::sync::Arc;
    use core::{
        ptr,
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    use anon_inodefs::AnonInodeFs;
    use kerrno::KError;
    use kpoll::{IoEvents, PollContext, PollRegisterError, PollSet, Pollable};
    use kvfs::{FMode, FileOperations, OpenFlags, VfsFile};
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
        InvalidRegister,
    }

    struct TestFile {
        kind: TestFileKind,
        poll_count: AtomicUsize,
        fail_register: AtomicBool,
        poll_set: PollSet,
    }

    impl TestFile {
        fn new(kind: TestFileKind) -> Self {
            Self {
                kind,
                poll_count: AtomicUsize::new(0),
                fail_register: AtomicBool::new(false),
                poll_set: PollSet::new(),
            }
        }

        fn poll_count(&self) -> usize {
            self.poll_count.load(Ordering::Acquire)
        }

        fn set_fail_register(&self, fail: bool) {
            self.fail_register.store(fail, Ordering::Release);
        }
    }

    impl Pollable for TestFile {
        fn poll(&self) -> IoEvents {
            match self.kind {
                TestFileKind::Ready | TestFileKind::Rewake => IoEvents::IN,
                TestFileKind::EmptyRewake
                | TestFileKind::PollSetBacked
                | TestFileKind::InvalidRegister => IoEvents::empty(),
                TestFileKind::Hup => IoEvents::HUP,
                TestFileKind::CountedOut => {
                    self.poll_count.fetch_add(1, Ordering::AcqRel);
                    IoEvents::OUT
                }
            }
        }

        fn register(
            &self,
            context: &mut PollContext<'_>,
            _events: IoEvents,
        ) -> Result<(), PollRegisterError> {
            if self.fail_register.load(Ordering::Acquire) {
                return Err(PollRegisterError::InvalidState);
            }
            match self.kind {
                TestFileKind::Rewake | TestFileKind::EmptyRewake => {
                    context.register(&self.poll_set)?;
                    self.poll_set.wake();
                }
                TestFileKind::PollSetBacked => {
                    context.register(&self.poll_set)?;
                }
                TestFileKind::InvalidRegister => return Err(PollRegisterError::InvalidState),
                _ => {}
            }
            Ok(())
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

        fn register_poll(
            &self,
            file: &VfsFile,
            context: &mut PollContext<'_>,
            events: IoEvents,
        ) -> Result<(), PollRegisterError> {
            Self::state(file).register(context, events)
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
                kcred::initial_cred(),
            )
            .expect("test file opens")
    }

    fn test_file_state(file: &VfsFile) -> Arc<TestFile> {
        TestFileFops::state(file)
    }

    #[def_test]
    fn test_epoll_creation() {
        let file = Epoll::new_file(kcred::initial_cred()).expect("epoll file opens");
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
            epoll
                .register_waker_only(
                    &interest,
                    IoEvents::empty(),
                    false,
                    interest.registered_events(),
                    false,
                )
                .unwrap();
        }

        assert!(epoll.inner.ready_queue.lock().len() <= 1);

        let mut out = [epoll_event { events: 0, data: 0 }; 1];
        assert_eq!(epoll.poll_events(&mut out), Err(KError::WouldBlock));
        assert_eq!(epoll.poll().bits(), IoEvents::empty().bits());
    }

    #[def_test]
    fn test_epoll_delete_nonexistent() {
        let epoll = Epoll::new();
        let dummy = Epoll::new_file(kcred::initial_cred()).expect("epoll file opens");
        assert!(epoll.delete(999, dummy).is_err());
    }

    #[def_test]
    fn test_epoll_modify_nonexistent() {
        let epoll = Epoll::new();
        let dummy = Epoll::new_file(kcred::initial_cred()).expect("epoll file opens");
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
    fn test_epoll_modify_updates_queued_interest_in_place() {
        let epoll = Epoll::new();
        let file = test_file(TestFileKind::Ready);

        epoll
            .add(
                3,
                file.clone(),
                EpollEvent {
                    events: IoEvents::IN,
                    user_data: 7,
                },
                EpollFlags::empty(),
            )
            .unwrap();
        epoll
            .modify(
                3,
                file.clone(),
                EpollEvent {
                    events: IoEvents::IN,
                    user_data: 9,
                },
                EpollFlags::empty(),
            )
            .unwrap();

        let mut out = [epoll_event { events: 0, data: 0 }; 1];
        assert_eq!(epoll.poll_events(&mut out).unwrap(), 1);
        assert_epoll_event(&out[0], IoEvents::IN.bits(), 9);
    }

    #[def_test]
    fn test_epoll_register_invalid_state_maps_to_invalid_input() {
        let epoll = Epoll::new();
        let file = test_file(TestFileKind::InvalidRegister);

        assert_eq!(
            epoll.add(
                3,
                file,
                EpollEvent {
                    events: IoEvents::IN,
                    user_data: 7,
                },
                EpollFlags::empty(),
            ),
            Err(KError::InvalidInput)
        );
    }

    #[def_test]
    fn test_epoll_failed_modify_preserves_previous_interest_config() {
        let epoll = Epoll::new();
        let file = test_file(TestFileKind::PollSetBacked);

        epoll
            .add(
                3,
                file.clone(),
                EpollEvent {
                    events: IoEvents::OUT,
                    user_data: 7,
                },
                EpollFlags::EDGE_TRIGGER,
            )
            .unwrap();

        test_file_state(&file).set_fail_register(true);
        assert_eq!(
            epoll.modify(
                3,
                file.clone(),
                EpollEvent {
                    events: IoEvents::IN,
                    user_data: 9,
                },
                EpollFlags::ONESHOT,
            ),
            Err(KError::InvalidInput)
        );

        let key = EntryKey::new(3, &file);
        let interest = epoll
            .inner
            .interests
            .lock()
            .get(&key)
            .cloned()
            .expect("interest must remain after failed modify");
        let event = interest.event_snapshot();
        assert_eq!(event.events.bits(), IoEvents::OUT.bits());
        assert_eq!(event.user_data, 7);
        assert!(matches!(interest.config.lock().mode, TriggerMode::Edge));
    }

    #[def_test]
    fn test_epoll_default() {
        let epoll = Epoll::default();
        assert!(epoll.poll().is_empty());
    }
}

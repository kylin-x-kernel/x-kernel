// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use anon_inodefs::AnonInodeFs;
use kerrno::KError;
use kpoll::{IoEvents, PollContext, PollRegisterError, PollSet, Pollable};
use ksync::Mutex;
use kvfs::{FMode, FileOperations, FilePollSource, OpenFlags, VfsFile};
use linux_raw_sys::general::{EPOLLET, EPOLLONESHOT};
use posix_types::k_epoll_event;
use unittest::def_test;

use super::{
    EntryKey, Epoll, EpollEvent, EpollFlags, EpollInterest, TriggerMode, match_ready_events,
    register_events,
};

fn assert_epoll_event(event: &k_epoll_event, events: u32, data: u64) {
    assert_eq!(event.events(), events);
    assert_eq!(event.data(), data);
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
    is_ready: AtomicBool,
    poll_set: PollSet,
}

impl TestFile {
    fn new(kind: TestFileKind) -> Self {
        Self {
            kind,
            poll_count: AtomicUsize::new(0),
            fail_register: AtomicBool::new(false),
            is_ready: AtomicBool::new(false),
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
            TestFileKind::EmptyRewake | TestFileKind::InvalidRegister => IoEvents::empty(),
            TestFileKind::PollSetBacked => {
                if self.is_ready.load(Ordering::Acquire) {
                    IoEvents::IN
                } else {
                    IoEvents::empty()
                }
            }
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
            // Exercise rollback even after a source installed a waiter.
            context.register(&self.poll_set)?;
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

struct CapturedSourceFile {
    source: Mutex<PollSet>,
    is_ready: AtomicBool,
    fail_capture: AtomicBool,
    captures: AtomicUsize,
    fallback_registrations: AtomicUsize,
}

impl CapturedSourceFile {
    fn replace_source(&self) -> PollSet {
        core::mem::replace(&mut *self.source.lock(), PollSet::new())
    }

    fn current_source(&self) -> PollSet {
        self.source.lock().clone()
    }
}

struct CapturedSourceFops;

impl FileOperations for CapturedSourceFops {
    fn poll(&self, file: &VfsFile) -> IoEvents {
        let state = file.private_data_get::<CapturedSourceFile>().unwrap();
        if state.is_ready.load(Ordering::Acquire) {
            IoEvents::IN
        } else {
            IoEvents::empty()
        }
    }

    fn capture_poll_source(
        &self,
        file: &VfsFile,
    ) -> Result<Option<FilePollSource>, PollRegisterError> {
        let state = file.private_data_get::<CapturedSourceFile>().unwrap();
        state.captures.fetch_add(1, Ordering::Relaxed);
        if state.fail_capture.load(Ordering::Acquire) {
            return Err(PollRegisterError::InvalidState);
        }
        Ok(Some(FilePollSource::new(
            state.current_source().downgrade(),
            IoEvents::IN,
        )))
    }

    fn register_poll(
        &self,
        file: &VfsFile,
        context: &mut PollContext<'_>,
        _events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        let state = file.private_data_get::<CapturedSourceFile>().unwrap();
        state.fallback_registrations.fetch_add(1, Ordering::Relaxed);
        context.register(&state.current_source())
    }
}

fn captured_source_file() -> (Arc<VfsFile>, Arc<CapturedSourceFile>) {
    let state = Arc::new(CapturedSourceFile {
        source: Mutex::new(PollSet::new()),
        is_ready: AtomicBool::new(false),
        fail_capture: AtomicBool::new(false),
        captures: AtomicUsize::new(0),
        fallback_registrations: AtomicUsize::new(0),
    });
    let file = AnonInodeFs::global()
        .get_file(
            "[epoll-captured-source-test]",
            Arc::new(CapturedSourceFops),
            state.clone(),
            FMode::READ | FMode::WRITE | FMode::STREAM,
            OpenFlags::empty(),
            kcred::initial_cred(),
        )
        .expect("captured-source test file opens");
    (file, state)
}

#[def_test]
fn test_epoll_captured_source_ready_add_and_no_in_mod() {
    for flags in [
        EpollFlags::empty(),
        EpollFlags::EDGE_TRIGGER,
        EpollFlags::ONESHOT,
    ] {
        for is_ready_at_add in [false, true] {
            let epoll = Epoll::new();
            let (file, state) = captured_source_file();
            state.is_ready.store(is_ready_at_add, Ordering::Release);
            let event = EpollEvent {
                events: IoEvents::IN,
                user_data: 7,
            };
            let add_event = EpollEvent {
                events: if is_ready_at_add {
                    IoEvents::IN
                } else {
                    IoEvents::OUT
                },
                ..event
            };
            epoll.add(3, file.clone(), add_event, flags).unwrap();
            assert_eq!(state.captures.load(Ordering::Relaxed), 1);
            let original = state.replace_source();
            assert_eq!(original.wake(), 0);
            let mut out = [k_epoll_event::default(); 1];
            if is_ready_at_add {
                assert_eq!(epoll.poll_events(&mut out), Ok(1));
                assert_epoll_event(&out[0], IoEvents::IN.bits(), 7);
            }
            state.is_ready.store(false, Ordering::Release);
            // This also rearms a fired ONESHOT without capturing again.
            epoll.modify(3, file.clone(), event, flags).unwrap();
            assert_eq!(epoll.poll_events(&mut out), Err(KError::WouldBlock));
            assert_eq!(state.current_source().wake(), 0);
            assert_eq!(original.wake(), 1);
            state.is_ready.store(true, Ordering::Release);
            assert_eq!(epoll.poll_events(&mut out), Ok(1));
            assert_epoll_event(&out[0], IoEvents::IN.bits(), 7);

            state.is_ready.store(false, Ordering::Release);
            assert_eq!(epoll.poll_events(&mut out), Err(KError::WouldBlock));
            if flags.contains(EpollFlags::ONESHOT) {
                assert_eq!(original.wake(), 0);
                epoll.modify(3, file.clone(), event, flags).unwrap();
            }
            // LT/ET rearm after consumption must retain the ADD source too.
            assert_eq!(state.current_source().wake(), 0);
            assert_eq!(original.wake(), 1);
            assert_eq!(state.captures.load(Ordering::Relaxed), 1);
            assert_eq!(state.fallback_registrations.load(Ordering::Relaxed), 0);
            epoll.delete(3, file).unwrap();
            assert_eq!(original.wake(), 0);
        }
    }
}

#[def_test]
fn test_epoll_dead_captured_source_never_rebinds() {
    for flags in [
        EpollFlags::empty(),
        EpollFlags::EDGE_TRIGGER,
        EpollFlags::ONESHOT,
    ] {
        let epoll = Epoll::new();
        let (file, state) = captured_source_file();
        let event = EpollEvent {
            events: IoEvents::IN,
            user_data: 7,
        };
        epoll.add(3, file.clone(), event, flags).unwrap();
        let original = state.replace_source();
        assert!(epoll.poll().is_empty());
        // No retained strong source owner: its final drop wakes the interest.
        drop(original);
        assert!(epoll.poll().contains(IoEvents::IN));
        let mut out = [k_epoll_event::default(); 1];
        assert_eq!(epoll.poll_events(&mut out), Err(KError::WouldBlock));
        assert!(epoll.poll().is_empty());
        epoll.modify(3, file.clone(), event, flags).unwrap();
        assert_eq!(state.current_source().wake(), 0);
        assert_eq!(state.captures.load(Ordering::Relaxed), 1);
        assert_eq!(state.fallback_registrations.load(Ordering::Relaxed), 0);
        assert_eq!(epoll.poll_events(&mut out), Err(KError::WouldBlock));

        // A dead subscription neither fabricates HUP nor disables current readiness.
        state.is_ready.store(true, Ordering::Release);
        epoll.modify(3, file.clone(), event, flags).unwrap();
        assert_eq!(epoll.poll_events(&mut out), Ok(1));
        assert_epoll_event(&out[0], IoEvents::IN.bits(), 7);
        epoll.delete(3, file.clone()).unwrap();
        state.is_ready.store(false, Ordering::Release);
        epoll.add(3, file.clone(), event, flags).unwrap();
        assert_eq!(state.captures.load(Ordering::Relaxed), 2);
        assert_eq!(state.current_source().wake(), 1);
        epoll.delete(3, file).unwrap();
    }
}

#[def_test]
fn test_epoll_capture_failure_and_duplicate_priority() {
    let epoll = Epoll::new();
    let (file, state) = captured_source_file();
    let event = EpollEvent {
        events: IoEvents::IN,
        user_data: 7,
    };
    state.fail_capture.store(true, Ordering::Release);
    assert_eq!(
        epoll.add(3, file.clone(), event, EpollFlags::empty()),
        Err(KError::InvalidInput)
    );
    assert!(epoll.inner.interests.lock().is_empty());
    assert_eq!(state.current_source().wake(), 0);
    state.fail_capture.store(false, Ordering::Release);
    epoll
        .add(3, file.clone(), event, EpollFlags::empty())
        .unwrap();
    state.fail_capture.store(true, Ordering::Release);
    assert_eq!(
        epoll.add(3, file.clone(), event, EpollFlags::empty()),
        Err(KError::AlreadyExists)
    );
    epoll
        .modify(3, file.clone(), event, EpollFlags::empty())
        .unwrap();
    assert_eq!(state.captures.load(Ordering::Relaxed), 2);
    epoll.delete(3, file).unwrap();
    assert_eq!(state.current_source().wake(), 0);
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
    let mut out = [k_epoll_event::default(); 4];
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
        None,
    ));
    interest.in_ready_queue.store(true, Ordering::Release);

    let weak = Arc::downgrade(&interest);
    {
        let mut queue = epoll.inner.ready_queue.lock();
        queue.push_back(weak.clone());
        queue.push_back(weak);
    }

    let mut out = [k_epoll_event::default(); 1];
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

    let mut out = [k_epoll_event::default(); 1];
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
        None,
    ));
    interest.in_ready_queue.store(true, Ordering::Release);

    epoll
        .inner
        .ready_queue
        .lock()
        .push_back(Arc::downgrade(&interest));

    let mut out = [k_epoll_event::default(); 1];
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
        None,
    ));
    interest.in_ready_queue.store(true, Ordering::Release);

    let weak = Arc::downgrade(&interest);
    {
        let mut queue = epoll.inner.ready_queue.lock();
        queue.push_back(weak.clone());
        queue.push_back(weak);
    }

    let mut out = [k_epoll_event::default(); 1];
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
        None,
    ));
    interest.in_ready_queue.store(true, Ordering::Release);

    epoll
        .inner
        .ready_queue
        .lock()
        .push_back(Arc::downgrade(&interest));

    let mut out = [k_epoll_event::default(); 1];
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
        None,
    ));
    interest.in_ready_queue.store(true, Ordering::Release);
    epoll
        .inner
        .ready_queue
        .lock()
        .push_back(Arc::downgrade(&interest));

    let mut out = [k_epoll_event::default(); 1];
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
        None,
    ));
    interest.in_ready_queue.store(true, Ordering::Release);

    epoll
        .inner
        .ready_queue
        .lock()
        .push_back(Arc::downgrade(&interest));

    let mut out = [k_epoll_event::default(); 1];
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
        None,
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

    let mut out = [k_epoll_event::default(); 1];
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

    let mut out = [k_epoll_event::default(); 1];
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
fn test_epoll_failed_modify_keeps_future_notifications() {
    for flags in [EpollFlags::empty(), EpollFlags::EDGE_TRIGGER] {
        let epoll = Epoll::new();
        let file = test_file(TestFileKind::PollSetBacked);
        let state = test_file_state(&file);
        epoll
            .add(
                3,
                file.clone(),
                EpollEvent {
                    events: IoEvents::IN,
                    user_data: 7,
                },
                flags,
            )
            .unwrap();
        state.set_fail_register(true);
        assert_eq!(
            epoll.modify(
                3,
                file.clone(),
                EpollEvent {
                    events: IoEvents::OUT,
                    user_data: 9,
                },
                EpollFlags::ONESHOT
            ),
            Err(KError::InvalidInput)
        );
        state.set_fail_register(false);
        state.is_ready.store(true, Ordering::Release);
        state.poll_set.wake();
        assert!(
            epoll.poll().contains(IoEvents::IN),
            "failed MOD must not strand the original interest"
        );
        let mut events = [k_epoll_event::default()];
        assert_eq!(epoll.poll_events(&mut events).unwrap(), 1);
        assert_epoll_event(&events[0], IoEvents::IN.bits(), 7);
    }
}

#[def_test]
fn test_epoll_failed_modify_retries_registration_and_cleans_up() {
    for flags in [
        EpollFlags::empty(),
        EpollFlags::EDGE_TRIGGER,
        EpollFlags::ONESHOT,
    ] {
        let epoll = Epoll::new();
        let file = test_file(TestFileKind::PollSetBacked);
        let state = test_file_state(&file);
        let event = EpollEvent {
            events: IoEvents::IN,
            user_data: 7,
        };
        epoll.add(3, file.clone(), event, flags).unwrap();
        let notifications = PollSet::new();
        let waker = core::task::Waker::from(Arc::new(notifications.clone()));
        let registration = epoll.inner.poll_ready.register(&waker).unwrap();
        state.set_fail_register(true);
        assert_eq!(
            epoll.modify(3, file.clone(), event, flags),
            Err(KError::InvalidInput)
        );
        // MOD recovery must wake existing epoll waiters, not merely enqueue.
        assert!(!registration.cancel());
        let mut events = [k_epoll_event::default()];
        for _ in 0..2 {
            assert!(epoll.poll().contains(IoEvents::IN));
            assert_eq!(epoll.poll_events(&mut events), Err(KError::InvalidInput));
        }
        state.set_fail_register(false);
        assert_eq!(epoll.poll_events(&mut events), Err(KError::WouldBlock));
        assert!(epoll.poll().is_empty());
        state.is_ready.store(true, Ordering::Release);
        state.poll_set.wake();
        assert!(epoll.poll().contains(IoEvents::IN));
        assert_eq!(epoll.poll_events(&mut events).unwrap(), 1);
        assert_epoll_event(&events[0], IoEvents::IN.bits(), 7);
        epoll.delete(3, file).unwrap();
        assert_eq!(
            state.poll_set.wake(),
            0,
            "all registrations must be cancelled"
        );
        assert_eq!(epoll.poll_events(&mut events), Err(KError::WouldBlock));
    }
}

#[def_test]
fn test_epoll_failed_modify_does_not_enable_fired_oneshot() {
    let epoll = Epoll::new();
    let file = test_file(TestFileKind::PollSetBacked);
    let state = test_file_state(&file);
    let event = EpollEvent {
        events: IoEvents::IN,
        user_data: 7,
    };
    state.is_ready.store(true, Ordering::Release);
    epoll
        .add(3, file.clone(), event, EpollFlags::ONESHOT)
        .unwrap();
    let mut events = [k_epoll_event::default()];
    assert_eq!(epoll.poll_events(&mut events).unwrap(), 1);
    state.is_ready.store(false, Ordering::Release);
    state.set_fail_register(true);
    assert_eq!(
        epoll.modify(3, file.clone(), event, EpollFlags::ONESHOT),
        Err(KError::InvalidInput)
    );
    state.set_fail_register(false);
    state.is_ready.store(true, Ordering::Release);
    assert_eq!(state.poll_set.wake(), 0);
    assert!(epoll.poll().is_empty());
    assert_eq!(epoll.poll_events(&mut events), Err(KError::WouldBlock));
    epoll
        .modify(3, file.clone(), event, EpollFlags::ONESHOT)
        .unwrap();
    assert_eq!(epoll.poll_events(&mut events).unwrap(), 1);
}

#[def_test]
fn test_epoll_failed_modify_preserves_queued_readiness() {
    for flags in [EpollFlags::empty(), EpollFlags::EDGE_TRIGGER] {
        let epoll = Epoll::new();
        let file = test_file(TestFileKind::PollSetBacked);
        let state = test_file_state(&file);
        let event = EpollEvent {
            events: IoEvents::IN,
            user_data: 7,
        };
        epoll.add(3, file.clone(), event, flags).unwrap();
        state.is_ready.store(true, Ordering::Release);
        state.poll_set.wake();
        state.set_fail_register(true);
        assert_eq!(
            epoll.modify(
                3,
                file.clone(),
                EpollEvent {
                    events: IoEvents::OUT,
                    user_data: 9,
                },
                EpollFlags::ONESHOT
            ),
            Err(KError::InvalidInput)
        );
        state.set_fail_register(false);
        let mut events = [k_epoll_event::default(); 2];
        assert_eq!(epoll.poll_events(&mut events).unwrap(), 1);
        assert_epoll_event(&events[0], IoEvents::IN.bits(), 7);
        state.is_ready.store(false, Ordering::Release);
        assert_eq!(epoll.poll_events(&mut events), Err(KError::WouldBlock));
    }
}

#[def_test]
fn test_epoll_default() {
    let epoll = Epoll::default();
    assert!(epoll.poll().is_empty());
}

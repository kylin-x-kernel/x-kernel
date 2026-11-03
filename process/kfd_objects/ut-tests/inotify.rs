// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::sync::atomic::{AtomicU32, Ordering};

use kvfs::VfsError;
use unittest::def_test;

/// Every watch stores `IN_UNMOUNT` as well: Linux ORs `FS_UNMOUNT` into each
/// mark unconditionally, so an assertion on a stored mask has to name it.
const UNMOUNT: u32 = linux_raw_sys::general::IN_UNMOUNT;

use super::*;

/// Each case owns its fake filesystem objects, including aliases of a path.
#[derive(Default)]
struct TestObjects {
    inodes: Mutex<HashMap<Vec<u8>, Arc<VfsInode>>>,
}

impl TestObjects {
    fn pin(&self, path: &[u8]) -> crate::inotify::Pin {
        self.inodes
            .lock()
            .entry(path.to_vec())
            .or_insert_with(pinned)
            .clone()
    }

    fn deliver(&self, path: &[u8], mask: u32, name: &[u8], cookie: u32) {
        crate::inotify::notify_path(&self.pin(path), mask, name, cookie);
    }
}

/// A stand-in for the watched object a real caller pins.
fn pinned() -> crate::inotify::Pin {
    kvfs::VfsInode::new_file_with_inode_operations(
        Arc::new(()),
        kvfs::empty_inode_operations(),
        kvfs::NodeFlags::empty(),
        kvfs::VfsInodeInit::new(
            kvfs::get_next_ino(),
            0,
            kvfs::Umode::new(kvfs::NodeType::RegularFile, kvfs::NodePermission::empty()),
        ),
    )
}

/// Stable fdinfo display numbers for a test pathname.
fn identity(path: &[u8]) -> WatchIdentity {
    let mut hash = 14_695_981_039_346_656_037u64; // FNV-1a over the path bytes
    for &byte in path {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(1_099_511_628_211);
    }
    WatchIdentity {
        device: 1,
        inode: hash,
    }
}

/// A uid no other case in this file uses.
///
/// The per-user instance and watch budgets live in one global ledger, and the
/// runner runs these cases concurrently: two cases sharing a uid would let the
/// one that deliberately spends a whole ceiling (`..._at_the_ceiling`) starve
/// a case that is asserting on its own successful reservations.
fn test_uid() -> kcred::Uid {
    static NEXT: AtomicU32 = AtomicU32::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

#[def_test]
fn add_watch_returns_ascending_descriptors() {
    let objects = TestObjects::default();
    let inotify = Inotify::new(test_uid()).unwrap();
    let first = inotify
        .add_watch(IN_ALL_EVENTS, identity(b"/etc"), objects.pin(b"/etc"))
        .unwrap();
    let second = inotify
        .add_watch(IN_ALL_EVENTS, identity(b"/tmp"), objects.pin(b"/tmp"))
        .unwrap();

    assert_eq!(first, FIRST_WATCH_DESCRIPTOR);
    assert_eq!(second, first + 1);
    assert_eq!(inotify.watch_count(), 2);
    assert_eq!(inotify.watch_identity(first), Some(identity(b"/etc")));
}

#[def_test]
fn descriptors_wrap_and_skip_occupied_numbers() {
    let objects = TestObjects::default();
    let inotify = Inotify::new(test_uid()).unwrap();
    // The counter is private, which is what lets this test place it at the wrap
    // boundary directly: with the counter at `i32::MAX`, the next registration
    // wraps to the first number -- never `i32::MAX` itself, and never `-1`, the
    // overrun record's sentinel.
    inotify
        .next_watch_descriptor
        .store(i32::MAX, Ordering::Relaxed);
    let first = inotify
        .add_watch(IN_ALL_EVENTS, identity(b"/wrap-1"), objects.pin(b"/wrap-1"))
        .unwrap();
    assert_eq!(first, FIRST_WATCH_DESCRIPTOR);
    let second = inotify
        .add_watch(IN_ALL_EVENTS, identity(b"/wrap-2"), objects.pin(b"/wrap-2"))
        .unwrap();
    assert_eq!(second, FIRST_WATCH_DESCRIPTOR + 1);

    // An occupied number is skipped rather than handed out twice: put the
    // counter back on the first watch's descriptor -- which is still live -- and
    // the next registration walks past both occupied numbers.
    inotify.next_watch_descriptor.store(first, Ordering::Relaxed);
    let third = inotify
        .add_watch(IN_ALL_EVENTS, identity(b"/wrap-3"), objects.pin(b"/wrap-3"))
        .unwrap();
    assert_eq!(third, second + 1);
    assert_eq!(inotify.watch_count(), 3);
}

#[def_test]
fn only_event_bits_are_stored_for_matching() {
    let objects = TestObjects::default();
    let inotify = Inotify::new(test_uid()).unwrap();

    // `IN_ISDIR` names an event *bit*, not a subscription to directories: Linux
    // keeps only `arg & IN_ALL_EVENTS` in the mark, so this watch subscribes to
    // no entry events at all -- the only thing its mask matches is the
    // `IN_UNMOUNT` every mark carries (asserted below).
    let none = inotify
        .add_watch(
            linux_raw_sys::general::IN_ISDIR,
            identity(b"/inotify-ut/isdir-only-marker"),
            objects.pin(b"/inotify-ut/isdir-only-marker"),
        )
        .unwrap();
    assert_eq!(inotify.watch_mask(none), Some(UNMOUNT));

    // The behavioural bits outlive the call, but live beside the mask rather
    // than inside it -- otherwise `IN_ONESHOT` would sit in the mask an event is
    // matched against.
    let descriptor = inotify
        .add_watch(
            linux_raw_sys::general::IN_CREATE | IN_EXCL_UNLINK | IN_ONESHOT,
            identity(b"/inotify-ut/flags-marker"),
            objects.pin(b"/inotify-ut/flags-marker"),
        )
        .unwrap();
    assert_eq!(
        inotify.watch_mask(descriptor),
        Some(linux_raw_sys::general::IN_CREATE | UNMOUNT)
    );
    assert_eq!(inotify.watch_flags(descriptor), Some(IN_EXCL_UNLINK | IN_ONESHOT));

    // `IN_MASK_ADD` merges both halves.
    let again = inotify
        .add_watch(
            IN_MASK_ADD | linux_raw_sys::general::IN_DELETE,
            identity(b"/inotify-ut/flags-marker"),
            objects.pin(b"/inotify-ut/flags-marker"),
        )
        .unwrap();
    assert_eq!(again, descriptor);
    assert_eq!(
        inotify.watch_mask(descriptor),
        Some(linux_raw_sys::general::IN_CREATE | linux_raw_sys::general::IN_DELETE | UNMOUNT)
    );
    assert_eq!(inotify.watch_flags(descriptor), Some(IN_EXCL_UNLINK | IN_ONESHOT));
}

#[def_test]
fn the_stored_mask_also_subscribes_to_unmount() {
    let objects = TestObjects::default();
    let inotify = Inotify::new(test_uid()).unwrap();
    // Linux ORs `FS_UNMOUNT` into every mark, so a watch that names only
    // event bits still matches it.
    let descriptor = inotify
        .add_watch(
            linux_raw_sys::general::IN_CREATE,
            identity(b"/inotify-ut/unmount-marker"),
            objects.pin(b"/inotify-ut/unmount-marker"),
        )
        .unwrap();
    assert_eq!(
        inotify.watch_mask(descriptor),
        Some(linux_raw_sys::general::IN_CREATE | UNMOUNT)
    );
}

#[def_test]
fn retiring_an_object_removes_its_watch_and_reports_it() {
    let objects = TestObjects::default();
    let inotify = Inotify::new(test_uid()).unwrap();
    let path = b"/inotify-ut/retire-marker";
    let descriptor = inotify
        .add_watch(IN_ALL_EVENTS, identity(path), objects.pin(path))
        .unwrap();
    objects.deliver(path, 0x100, b"before", 0);

    let inode = objects.pin(path);
    crate::inotify::retire_entry(&inode, linux_raw_sys::general::IN_DELETE_SELF, false);

    // The watch is gone from both the instance and the registry, so a recycled
    // inode number cannot hand it to a new object.
    assert_eq!(inotify.watch_count(), 0);
    assert_eq!(crate::inotify::registry_entry_count(&inode), 0);

    // One drain returns all three records in order: the event queued before the
    // retirement, then `IN_DELETE_SELF` (the watch subscribed to it), then the
    // `IN_IGNORED` every retired watch gets.
    let mut buf = [0u8; 64];
    assert_eq!(inotify.drain_events(&mut buf).unwrap(), 64);
    let mask_at = |offset: usize| {
        u32::from_ne_bytes([buf[offset + 4], buf[offset + 5], buf[offset + 6], buf[offset + 7]])
    };
    let wd_at = |offset: usize| {
        i32::from_ne_bytes([buf[offset], buf[offset + 1], buf[offset + 2], buf[offset + 3]])
    };
    assert_eq!(wd_at(0), descriptor);
    assert_eq!(mask_at(0), 0x100);
    assert_eq!(wd_at(32), descriptor);
    assert_eq!(mask_at(32), linux_raw_sys::general::IN_DELETE_SELF);
    assert_eq!(wd_at(48), descriptor);
    assert_eq!(mask_at(48), linux_raw_sys::general::IN_IGNORED);

    // Nothing else arrives for that identity, and nothing is left to drain.
    objects.deliver(path, 0x100, b"after", 0);
    assert_eq!(inotify.drain_events(&mut buf), Err(KError::WouldBlock));
}

#[def_test]
fn re_watching_an_identity_through_another_spelling_updates_one_watch() {
    let objects = TestObjects::default();
    let inotify = Inotify::new(test_uid()).unwrap();
    let first = inotify
        .add_watch(0x2, identity(b"/etc"), objects.pin(b"/etc"))
        .unwrap();

    // The same object under a different spelling: Linux finds the mark on the
    // inode (`fsnotify_find_mark()`) and returns its descriptor with the mask
    // updated, rather than adding a second watch for the same object.
    let again = inotify
        .add_watch(0x4, identity(b"/etc"), objects.pin(b"/etc"))
        .unwrap();
    assert_eq!(first, again);
    assert_eq!(inotify.watch_count(), 1);
    assert_eq!(inotify.watch_mask(first), Some(0x4 | UNMOUNT));

    // A different object still gets its own watch.
    let other = inotify
        .add_watch(0x2, identity(b"/etc-other"), objects.pin(b"/etc-other"))
        .unwrap();
    assert_ne!(other, first);
    assert_eq!(inotify.watch_count(), 2);
}

#[def_test]
fn add_watch_replaces_mask_without_mask_add() {
    let objects = TestObjects::default();
    let inotify = Inotify::new(test_uid()).unwrap();
    let descriptor = inotify
        .add_watch(IN_ALL_EVENTS, identity(b"/etc"), objects.pin(b"/etc"))
        .unwrap();
    let again = inotify
        .add_watch(0x2, identity(b"/etc"), objects.pin(b"/etc"))
        .unwrap();

    assert_eq!(descriptor, again);
    assert_eq!(inotify.watch_count(), 1);
    assert_eq!(inotify.watch_mask(descriptor), Some(0x2 | UNMOUNT));
}

#[def_test]
fn add_watch_with_mask_add_merges_bits() {
    let objects = TestObjects::default();
    let inotify = Inotify::new(test_uid()).unwrap();
    let descriptor = inotify
        .add_watch(0x2, identity(b"/etc"), objects.pin(b"/etc"))
        .unwrap();
    let again = inotify
        .add_watch(IN_MASK_ADD | 0x4, identity(b"/etc"), objects.pin(b"/etc"))
        .unwrap();

    assert_eq!(descriptor, again);
    assert_eq!(inotify.watch_mask(descriptor), Some(0x6 | UNMOUNT));
}

#[def_test]
fn add_watch_with_mask_create_rejects_duplicate() {
    let objects = TestObjects::default();
    let inotify = Inotify::new(test_uid()).unwrap();
    inotify
        .add_watch(IN_ALL_EVENTS, identity(b"/etc"), objects.pin(b"/etc"))
        .unwrap();

    assert_eq!(
        inotify.add_watch(
            IN_MASK_CREATE | IN_ALL_EVENTS,
            identity(b"/etc"),
            objects.pin(b"/etc")
        ),
        Err(KError::AlreadyExists)
    );
}

#[def_test]
fn add_watch_rejects_unknown_bits() {
    let objects = TestObjects::default();
    let inotify = Inotify::new(test_uid()).unwrap();

    // The path argument is bounded by the adapter -- the same bytes Linux
    // bounds -- so the object itself checks only the mask domain.
    assert_eq!(
        inotify.add_watch(0x0001_0000, identity(b"/etc"), objects.pin(b"/etc")),
        Err(KError::InvalidInput)
    );
    assert_eq!(
        inotify.add_watch(1 << 16, identity(b"/etc"), objects.pin(b"/etc")),
        Err(KError::InvalidInput)
    );
}

#[def_test]
fn add_watch_masks_are_confined_to_event_bits() {
    let objects = TestObjects::default();
    let inotify = Inotify::new(test_uid()).unwrap();
    let descriptor = inotify
        .add_watch(IN_ALL_EVENTS, identity(b"/etc"), objects.pin(b"/etc"))
        .unwrap();

    assert_eq!(inotify.watch_mask(descriptor), Some(IN_ALL_EVENTS | UNMOUNT));
}

#[def_test]
fn add_watch_reports_storage_full_at_the_ceiling() {
    let objects = TestObjects::default();
    let inotify = Inotify::new(test_uid()).unwrap();
    for index in 0..MAX_WATCHES {
        let path = alloc::format!("/w{index}");
        inotify
            .add_watch(
                IN_ALL_EVENTS,
                identity(path.as_bytes()),
                objects.pin(path.as_bytes()),
            )
            .unwrap();
    }

    assert_eq!(
        inotify.add_watch(
            IN_ALL_EVENTS,
            identity(b"/overflow"),
            objects.pin(b"/overflow")
        ),
        Err(KError::StorageFull)
    );
    assert_eq!(inotify.watch_count(), MAX_WATCHES);
}

#[def_test]
fn remove_watch_reports_missing_descriptor() {
    let objects = TestObjects::default();
    let inotify = Inotify::new(test_uid()).unwrap();
    let descriptor = inotify
        .add_watch(IN_ALL_EVENTS, identity(b"/etc"), objects.pin(b"/etc"))
        .unwrap();

    assert_eq!(inotify.remove_watch(descriptor), Ok(()));
    assert_eq!(inotify.remove_watch(descriptor), Err(KError::InvalidInput));
    assert_eq!(inotify.watch_count(), 0);
}

#[def_test]
fn remove_watch_queues_in_ignored() {
    let objects = TestObjects::default();
    let inotify = Inotify::new(test_uid()).unwrap();
    let descriptor = inotify
        .add_watch(IN_ALL_EVENTS, identity(b"/etc"), objects.pin(b"/etc"))
        .unwrap();
    inotify.remove_watch(descriptor).unwrap();

    assert!(inotify.poll().contains(IoEvents::IN));
    let mut buf = [0u8; 16];
    let written = inotify.drain_events(&mut buf).unwrap();
    assert_eq!(written, 16);
    assert_eq!(i32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]), descriptor);
    assert_eq!(
        u32::from_ne_bytes([buf[4], buf[5], buf[6], buf[7]]),
        linux_raw_sys::general::IN_IGNORED
    );
    // The record is consumed: no readiness, and a further drain reports empty.
    assert!(!inotify.poll().contains(IoEvents::IN));
    assert_eq!(inotify.drain_events(&mut buf), Err(KError::WouldBlock));
}

#[def_test]
fn poll_reports_in_only_with_a_readable_record() {
    let objects = TestObjects::default();
    let inotify = Inotify::new(test_uid()).unwrap();
    inotify
        .add_watch(IN_ALL_EVENTS, identity(b"/etc"), objects.pin(b"/etc"))
        .unwrap();

    assert!(!inotify.poll().contains(IoEvents::IN));
    inotify.queue_event(FIRST_WATCH_DESCRIPTOR, 0x2, b"file", 0);
    assert!(inotify.poll().contains(IoEvents::IN));
}

/// Restores the live queued-event ceiling when a serial test leaves, including
/// on an assertion failure, so parallel cases still see the default.
struct RestoreQueuedEvents(usize);

impl Drop for RestoreQueuedEvents {
    fn drop(&mut self) {
        let value = alloc::format!("{}", self.0);
        let _ = crate::inotify_limits::InotifyLimit::QueuedEvents
            .write(value.as_bytes(), &kcred::initial_cred());
    }
}

#[def_test(serial)]
fn queued_events_limit_snapshots_for_new_instances() {
    let original = crate::inotify_limits::InotifyLimit::QueuedEvents.value();
    let restore = RestoreQueuedEvents(original);
    assert_eq!(
        crate::inotify_limits::InotifyLimit::QueuedEvents
            .write(b"4", &kcred::initial_cred()),
        Ok(())
    );

    // The instance snapshots the live value at creation, not the compiled-in
    // default or a value read on every push.
    let inotify = Inotify::new(50_000).unwrap();
    assert_eq!(inotify.max_queued_events, 4);

    // Raise the global ceiling after creation. A push that read the live
    // global would now admit the fifth event instead of overflowing the
    // snapshotted four-record ceiling.
    assert_eq!(
        crate::inotify_limits::InotifyLimit::QueuedEvents
            .write(b"8", &kcred::initial_cred()),
        Ok(())
    );
    assert_eq!(crate::inotify_limits::InotifyLimit::QueuedEvents.value(), 8);

    // Four records fill the snapshotted ceiling; the fifth push queues only
    // the overrun marker, proving the instance uses its creation-time value.
    for _ in 0..4 {
        inotify.queue_event(1, 0x2, &[], 0);
    }
    inotify.queue_event(1, 0x2, b"dropped", 0);
    assert_eq!(inotify.queued_bytes(), 5 * EVENT_HEADER_BYTES);
    drop(inotify);
    drop(restore);

    // A later instance snapshots the restored value, showing that the field
    // is creation-time state rather than a live global read.
    let fresh = Inotify::new(50_001).unwrap();
    assert_eq!(fresh.max_queued_events, original);
}

#[def_test]
fn queue_overflow_queues_the_overrun_record_immediately() {
    let inotify = Inotify::new(test_uid()).unwrap();
    let fill = |inotify: &Arc<Inotify>| {
        for _ in 0..MAX_QUEUED_EVENTS {
            // Nameless records: each occupies exactly the 16-byte header.
            inotify.queue_event(1, 0x2, &[], 0);
        }
    };
    fill(&inotify);

    // Linux appends its pre-allocated overflow event the moment a push finds
    // the queue at its ceiling (`fsnotify_insert_event` jumps to `queue` with
    // `q_len++`), so the record takes a slot of its own rather than waiting for
    // a drain that a continuously busy producer never allows.
    inotify.queue_event(1, 0x2, b"dropped", 0);
    assert_eq!(inotify.queued_bytes(), (MAX_QUEUED_EVENTS + 1) * 16);

    // Further overflowing pushes do not queue a second record: Linux queues the
    // overflow event only while it is not already queued.
    inotify.queue_event(1, 0x2, b"dropped-again", 0);
    assert_eq!(inotify.queued_bytes(), (MAX_QUEUED_EVENTS + 1) * 16);

    // It is the last member, so a reader sees the loss in order.
    let mut big = alloc::vec![0u8; (MAX_QUEUED_EVENTS + 1) * 16];
    assert_eq!(inotify.drain_events(&mut big).unwrap(), big.len());
    let last = big.len() - EVENT_HEADER_BYTES;
    assert_eq!(
        i32::from_ne_bytes([big[last], big[last + 1], big[last + 2], big[last + 3]]),
        -1
    );
    assert_eq!(
        u32::from_ne_bytes([big[last + 4], big[last + 5], big[last + 6], big[last + 7]]),
        linux_raw_sys::general::IN_Q_OVERFLOW
    );
    assert_eq!(big[last + 12..last + 16], [0, 0, 0, 0]);
    assert!(!inotify.poll().contains(IoEvents::IN));

    // Consuming it re-arms overrun reporting, and the queue takes events again.
    inotify.queue_event(1, 0x2, b"kept", 0);
    assert_eq!(inotify.queued_bytes(), 32);
    fill(&inotify);
    inotify.queue_event(1, 0x2, b"dropped-again", 0);
    // The ceiling is reached again, a fresh overrun record is queued, and the
    // 32-byte `kept` record is still counted with it.
    assert_eq!(inotify.queued_bytes(), (MAX_QUEUED_EVENTS + 1) * 16 + 16);
}

#[def_test]
fn drain_returns_the_copied_bytes_when_the_next_record_does_not_fit() {
    let inotify = Inotify::new(test_uid()).unwrap();
    inotify.queue_event(1, 0x2, b"first", 0);
    inotify.queue_event(1, 0x2, b"second", 0);

    // Both names round up to the 16-byte field, so each record is 32 bytes. A
    // 48-byte buffer holds the first whole record and cannot hold the second,
    // and Linux ends such a read with the byte count it already copied
    // (`inotify_read`: `if (start != buf && ret != -EFAULT) ret = buf - start`)
    // rather than with EINVAL.
    let mut buf = [0u8; 48];
    assert_eq!(inotify.drain_events(&mut buf).unwrap(), 32);
    assert_eq!(inotify.drain_events(&mut buf).unwrap(), 32);
    assert_eq!(inotify.drain_events(&mut buf), Err(KError::WouldBlock));
}

#[def_test]
fn add_watch_rejects_both_add_directives_together() {
    let objects = TestObjects::default();
    let inotify = Inotify::new(test_uid()).unwrap();

    // Linux: "IN_MASK_ADD and IN_MASK_CREATE don't make sense together".
    assert_eq!(
        inotify.add_watch(
            IN_ALL_EVENTS | IN_MASK_ADD | IN_MASK_CREATE,
            identity(b"/etc"),
            objects.pin(b"/etc")
        ),
        Err(KError::InvalidInput)
    );
    assert_eq!(inotify.watch_count(), 0);
}

#[def_test]
fn drain_rejects_a_buffer_shorter_than_the_next_record() {
    let inotify = Inotify::new(test_uid()).unwrap();
    inotify.queue_event(1, 0x2, b"a-long-name", 0);

    let mut buf = [0u8; 16];
    assert_eq!(inotify.drain_events(&mut buf), Err(KError::InvalidInput));
    // "a-long-name" is 11 bytes: 12 rounded up to 16, so the record is 32.
    let mut enough = [0u8; 32];
    assert_eq!(inotify.drain_events(&mut enough).unwrap(), 32);
}

#[def_test]
fn drain_pads_names_to_the_event_header_boundary() {
    let inotify = Inotify::new(test_uid()).unwrap();
    inotify.queue_event(1, 0x2, b"abc", 0);

    // "abc" + NUL is 4 bytes, rounded up to the 16-byte event header: the
    // record is 16 + 16 = 32 bytes and `len` reports the rounded 16.
    let mut buf = [0u8; 32];
    assert_eq!(inotify.drain_events(&mut buf).unwrap(), 32);
    assert_eq!(u32::from_ne_bytes([buf[12], buf[13], buf[14], buf[15]]), 16);
    assert_eq!(&buf[16..20], b"abc\0");
    assert_eq!(&buf[20..32], &[0u8; 12]);
}

#[def_test]
fn empty_destination_read_consults_the_queue_first() {
    let file = Inotify::new_file(0, kcred::initial_cred()).expect("inotify file opens");
    let fops = InotifyFops;
    let state = Inotify::from_file(&file).expect("inotify file");
    state.queue_event(1, 0x2, b"", 0);

    // Linux's `get_one_event` compares the queued record size with `count`
    // before copying, so a record that cannot fit makes even a zero-length
    // read `EINVAL`. Exercise both the `read` hook used by the wait path and
    // `read_from`, which reaches `read_iter` the way a user read does.
    assert_eq!(fops.read(&file, &mut [], 0), Err(KError::InvalidInput));
    assert_eq!(
        file.read_from(&mut [], &mut 0),
        Err(VfsError::InvalidInput)
    );

    // An empty queue instead waits (or, non-blocking, reports `EAGAIN`).
    let empty = Inotify::new_file(0, kcred::initial_cred()).expect("inotify file opens");
    empty.set_nonblocking(true);
    assert_eq!(fops.read(&empty, &mut [], 0), Err(KError::WouldBlock));
    assert_eq!(
        empty.read_from(&mut [], &mut 0),
        Err(VfsError::WouldBlock)
    );
}

#[def_test]
fn new_file_rejects_unknown_open_flags() {
    assert!(Inotify::new_file(1 << 30, kcred::initial_cred()).is_err());
}

#[def_test]
fn nonblocking_read_reports_would_block() {
    let file = Inotify::new_file(0, kcred::initial_cred()).expect("inotify file opens");
    file.set_nonblocking(true);

    let mut buf = [0u8; 64];
    let mut pos = 0;
    assert_eq!(file.read_from(&mut buf, &mut pos), Err(KError::WouldBlock));
}

#[def_test]
fn watch_state_is_reachable_through_file_private_data() {
    let objects = TestObjects::default();
    let file = Inotify::new_file(0, kcred::initial_cred()).expect("inotify file opens");
    let inotify = Inotify::from_file(&file).expect("inotify file exposes its state");
    let descriptor = inotify
        .add_watch(IN_ALL_EVENTS, identity(b"/etc"), objects.pin(b"/etc"))
        .unwrap();

    assert_eq!(inotify.watch_mask(descriptor), Some(IN_ALL_EVENTS | UNMOUNT));
}

#[def_test]
fn linux_mask_flags_are_accepted_and_stored() {
    let objects = TestObjects::default();
    let inotify = Inotify::new(test_uid()).unwrap();
    // Linux accepts the behavioural flags; refusing them would fail calls
    // that work there. The stored mask keeps them, minus the add directives.
    let descriptor = inotify
        .add_watch(
            linux_raw_sys::general::IN_CREATE
                | linux_raw_sys::general::IN_ONLYDIR
                | linux_raw_sys::general::IN_DONT_FOLLOW,
            identity(b"/inotify-ut/mask-flags-marker"),
            objects.pin(b"/inotify-ut/mask-flags-marker"),
        )
        .unwrap();
    let stored = inotify.watch_mask(descriptor).unwrap();
    assert_eq!(stored, linux_raw_sys::general::IN_CREATE | UNMOUNT);
    // The lookup flags are applied while resolving, not stored: Linux keeps
    // only `arg & IN_ALL_EVENTS` in the mark, so a stored `IN_ONLYDIR` would
    // only ever be compared against events that never carry it.
    assert_eq!(stored & linux_raw_sys::general::IN_ONLYDIR, 0);
    assert_eq!(stored & linux_raw_sys::general::IN_DONT_FOLLOW, 0);

    // A bit outside Linux's domain is still refused.
    assert_eq!(
        inotify.add_watch(
            1 << 17,
            identity(b"/inotify-ut/mask-bad-marker"),
            objects.pin(b"/inotify-ut/mask-bad-marker")
        ),
        Err(KError::InvalidInput)
    );
    assert_eq!(
        inotify.add_watch(
            1 << 20,
            identity(b"/inotify-ut/mask-bad-marker"),
            objects.pin(b"/inotify-ut/mask-bad-marker")
        ),
        Err(KError::InvalidInput)
    );
}

#[def_test]
fn fs_event_reaches_the_registered_watch() {
    let objects = TestObjects::default();
    let inotify = Inotify::new(test_uid()).unwrap();
    let path = b"/inotify-ut/fs-event-marker";
    let descriptor = inotify
        .add_watch(IN_ALL_EVENTS, identity(path), objects.pin(path))
        .unwrap();

    objects.deliver(path, 0x100, b"created-file", 0);

    assert!(inotify.poll().contains(IoEvents::IN));
    let mut buf = [0u8; 64];
    let written = inotify.drain_events(&mut buf).unwrap();
    // "created-file" is 12 bytes: the NUL-inclusive 13 rounds up to the
    // 16-byte header, so `len` reports 16 and the record is 32 bytes.
    assert_eq!(written, 32);
    assert_eq!(
        i32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]),
        descriptor
    );
    assert_eq!(u32::from_ne_bytes([buf[4], buf[5], buf[6], buf[7]]), 0x100);
    assert_eq!(u32::from_ne_bytes([buf[12], buf[13], buf[14], buf[15]]), 16);
    assert_eq!(&buf[16..28], b"created-file");
    assert_eq!(buf[28], 0);
}

#[def_test]
fn fs_event_reports_the_rename_cookie() {
    let objects = TestObjects::default();
    let inotify = Inotify::new(test_uid()).unwrap();
    let path = b"/inotify-ut/cookie-marker";
    inotify
        .add_watch(IN_ALL_EVENTS, identity(path), objects.pin(path))
        .unwrap();

    // Both halves of a move carry the same cookie in the record's third field;
    // every other event carries 0 there.
    objects.deliver(path, 0x40, b"moved", 0x1234_5678);
    let mut buf = [0u8; 32];
    assert_eq!(inotify.drain_events(&mut buf).unwrap(), 32);
    assert_eq!(
        u32::from_ne_bytes([buf[8], buf[9], buf[10], buf[11]]),
        0x1234_5678
    );

    objects.deliver(path, 0x100, b"created", 0);
    assert_eq!(inotify.drain_events(&mut buf).unwrap(), 32);
    assert_eq!(u32::from_ne_bytes([buf[8], buf[9], buf[10], buf[11]]), 0);
}

#[def_test]
fn fs_event_after_removal_never_carries_the_descriptor() {
    let objects = TestObjects::default();
    let inotify = Inotify::new(test_uid()).unwrap();
    let path = b"/inotify-ut/removed-marker";
    let descriptor = inotify
        .add_watch(IN_ALL_EVENTS, identity(path), objects.pin(path))
        .unwrap();
    inotify.remove_watch(descriptor).unwrap();

    // The subscription is re-checked under the watches lock, so a delivery that
    // overlaps the removal cannot queue an event for a descriptor that is gone.
    objects.deliver(path, 0x100, b"late", 0);
    let mut buf = [0u8; 64];
    // Only the IN_IGNORED the removal queued is readable.
    let written = inotify.drain_events(&mut buf).unwrap();
    assert_eq!(written, 16);
    assert_eq!(
        u32::from_ne_bytes([buf[4], buf[5], buf[6], buf[7]]),
        linux_raw_sys::general::IN_IGNORED
    );
    assert_eq!(inotify.drain_events(&mut buf), Err(KError::WouldBlock));
}

#[def_test]
fn fs_event_respects_the_watch_mask() {
    let objects = TestObjects::default();
    let inotify = Inotify::new(test_uid()).unwrap();
    let path = b"/inotify-ut/fs-mask-marker";
    let descriptor = inotify
        .add_watch(0x100, identity(path), objects.pin(path))
        .unwrap();

    // IN_DELETE (0x200) was not subscribed: nothing is queued.
    objects.deliver(path, 0x200, b"gone", 0);
    assert!(!inotify.poll().contains(IoEvents::IN));
    assert_eq!(inotify.queued_bytes(), 0);

    // The subscribed bit queues under the same watch. "made" is 4 bytes: the
    // NUL-inclusive 5 rounds up to 16, so the record is 32 bytes.
    objects.deliver(path, 0x100, b"made", 0);
    let mut buf = [0u8; 32];
    assert_eq!(inotify.drain_events(&mut buf).unwrap(), 32);
    assert_eq!(
        i32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]),
        descriptor
    );
    assert_eq!(u32::from_ne_bytes([buf[12], buf[13], buf[14], buf[15]]), 16);
}

#[def_test]
fn watch_removal_stops_fs_delivery() {
    let objects = TestObjects::default();
    let inotify = Inotify::new(test_uid()).unwrap();
    let path = b"/inotify-ut/fs-remove-marker";
    let descriptor = inotify
        .add_watch(IN_ALL_EVENTS, identity(path), objects.pin(path))
        .unwrap();
    inotify.remove_watch(descriptor).unwrap();

    // The unregistered watch no longer receives events; only IN_IGNORED is
    // pending from the removal itself.
    objects.deliver(path, 0x100, b"after", 0);
    let mut buf = [0u8; 32];
    assert_eq!(inotify.drain_events(&mut buf).unwrap(), 16);
    assert_eq!(
        i32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]),
        descriptor
    );
    assert_eq!(
        u32::from_ne_bytes([buf[4], buf[5], buf[6], buf[7]]),
        linux_raw_sys::general::IN_IGNORED
    );
    assert_eq!(u32::from_ne_bytes([buf[12], buf[13], buf[14], buf[15]]), 0);
    assert!(!inotify.poll().contains(IoEvents::IN));
}

#[def_test]
fn registry_does_not_pin_dead_instances() {
    let objects = TestObjects::default();
    let path = b"/inotify-ut/fs-dead-marker";
    let dead = Inotify::new(test_uid()).unwrap();
    dead.add_watch(IN_ALL_EVENTS, identity(path), objects.pin(path))
        .unwrap();
    drop(dead);

    // A fresh instance on the same path still delivers, and the dead entry
    // does not panic the upgrade.
    let alive = Inotify::new(test_uid()).unwrap();
    let descriptor = alive
        .add_watch(IN_ALL_EVENTS, identity(path), objects.pin(path))
        .unwrap();
    objects.deliver(path, 0x100, b"live", 0);

    // "live" is 4 bytes: the NUL-inclusive 5 rounds up to 16, record 32.
    let mut buf = [0u8; 32];
    assert_eq!(alive.drain_events(&mut buf).unwrap(), 32);
    assert_eq!(
        i32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]),
        descriptor
    );
}

#[def_test]
fn empty_mask_is_rejected_but_flags_alone_are_accepted() {
    let objects = TestObjects::default();
    let inotify = Inotify::new(test_uid()).unwrap();
    // Linux: "the given event mask contains no valid events" is EINVAL; a
    // watch that subscribes to nothing must not consume the budget.
    assert_eq!(
        inotify.add_watch(
            0,
            identity(b"/inotify-ut/empty-mask-marker"),
            objects.pin(b"/inotify-ut/empty-mask-marker")
        ),
        Err(KError::InvalidInput)
    );
    assert_eq!(inotify.watch_count(), 0);
    // Flags alone are accepted, matching Linux's "at least one valid bit"
    // (its check is against ALL_INOTIFY_BITS, not against the event bits).
    assert!(
        inotify
            .add_watch(
                linux_raw_sys::general::IN_ONLYDIR,
                identity(b"/inotify-ut/flags-only-marker"),
                objects.pin(b"/inotify-ut/flags-only-marker")
            )
            .is_ok()
    );
    assert_eq!(inotify.watch_count(), 1);
}

#[def_test]
fn two_instances_watching_one_path_keep_separate_entries() {
    let objects = TestObjects::default();
    let path = b"/inotify-ut/shared-path-marker";
    let first = Inotify::new(test_uid()).unwrap();
    let second = Inotify::new(test_uid()).unwrap();
    let first_wd = first
        .add_watch(IN_ALL_EVENTS, identity(path), objects.pin(path))
        .unwrap();
    let second_wd = second
        .add_watch(IN_ALL_EVENTS, identity(path), objects.pin(path))
        .unwrap();
    assert_eq!(first_wd, second_wd);
    assert_eq!(crate::inotify::registry_entry_count(&objects.pin(path)), 2);
    first.remove_watch(first_wd).unwrap();
    assert_eq!(crate::inotify::registry_entry_count(&objects.pin(path)), 1);
    objects.deliver(path, 0x100, b"still-live", 0);
    let mut buf = [0u8; 32];
    assert_eq!(second.drain_events(&mut buf).unwrap(), 32);
    assert_eq!(i32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]), second_wd);
}

#[def_test]
fn read_iter_returns_a_full_chunk_without_waiting_for_the_next_event() {
    // A BLOCKING file: the hang the override fixes only happens there (the
    // nonblocking variant would break the chunked default loop with WouldBlock
    // and pass either way). A regression back to the default turns this read
    // into a wait for a second event, which the test runner reports as a
    // timeout.
    let file = Inotify::new_file(0, kcred::initial_cred()).expect("inotify file opens");
    let inotify = Inotify::from_file(&file).unwrap();
    // 256 nameless 16-byte records sum to exactly one 4 KiB chunk: the
    // page-chunked default read_iter looped into a second blocking wait right
    // here, hanging the read until an unrelated event arrived. The override
    // must return the copied bytes instead, and a nonblocking follow-up read
    // must report the dry queue rather than wait.
    for _ in 0..256 {
        inotify.queue_event(1, linux_raw_sys::general::IN_IGNORED, b"", 0);
    }
    let mut buf = [0u8; 8192];
    let mut pos = 0;
    assert_eq!(file.read_from(&mut buf, &mut pos).unwrap(), 4096);
}

#[def_test]
fn dropping_an_instance_clears_its_registry_entries() {
    let objects = TestObjects::default();
    let path = b"/inotify-ut/drop-marker";
    let instance = Inotify::new(test_uid()).unwrap();
    instance
        .add_watch(IN_ALL_EVENTS, identity(path), objects.pin(path))
        .unwrap();
    assert_eq!(crate::inotify::registry_entry_count(&objects.pin(path)), 1);

    // Closing the instance (dropping the object) must release the path key;
    // otherwise the registry would grow once per distinct path ever watched.
    drop(instance);
    assert_eq!(crate::inotify::registry_entry_count(&objects.pin(path)), 0);
    objects.deliver(path, 0x100, b"after-drop", 0);
}

#[def_test]
fn mask_add_and_mask_create_together_are_rejected() {
    let objects = TestObjects::default();
    let inotify = Inotify::new(test_uid()).unwrap();
    assert_eq!(
        inotify.add_watch(
            IN_MASK_ADD | IN_MASK_CREATE | 0x100,
            identity(b"/inotify-ut/mask-conflict-marker"),
            objects.pin(b"/inotify-ut/mask-conflict-marker")
        ),
        Err(KError::InvalidInput)
    );
    // Neither bit alone is a conflict.
    assert!(
        inotify
            .add_watch(
                IN_MASK_ADD | 0x100,
                identity(b"/inotify-ut/mask-conflict-marker"),
                objects.pin(b"/inotify-ut/mask-conflict-marker")
            )
            .is_ok()
    );
}

#[def_test]
fn read_iter_keeps_partially_copied_records() {
    let file = Inotify::new_file(0, kcred::initial_cred()).expect("inotify file opens");
    let inotify = Inotify::from_file(&file).unwrap();
    inotify.queue_event(1, 0x2, b"first", 0);
    inotify.queue_event(1, 0x2, b"second", 0);

    // Records are 32 bytes each. A 48-byte buffer takes the first whole record
    // and cannot hold the second: the copied bytes must be returned rather than
    // discarded with EINVAL, and the second record must stay queued.
    let mut buf = [0u8; 48];
    let mut pos = 0;
    assert_eq!(file.read_from(&mut buf, &mut pos).unwrap(), 32);
    assert_eq!(file.read_from(&mut buf, &mut pos).unwrap(), 32);
}

#[def_test]
fn routing_uses_pinned_objects_instead_of_display_numbers() {
    let instance = Inotify::new(test_uid()).unwrap();
    let first = pinned();
    let second = pinned();
    let display = identity(b"/inotify-ut/equal-display-numbers");
    let first_wd = instance.add_watch(0x100, display, first.clone()).unwrap();
    let second_wd = instance.add_watch(0x100, display, second.clone()).unwrap();
    assert_ne!(first_wd, second_wd);
    // A different mount's display metadata must still update the same object.
    let alias = WatchIdentity {
        device: display.device + 1,
        inode: display.inode,
    };
    assert_eq!(
        instance
            .add_watch(IN_MASK_ADD | 0x2, alias, first.clone())
            .unwrap(),
        first_wd
    );
    crate::inotify::notify_path(&second, 0x100, b"second", 0);
    let mut bytes = [0u8; 64];
    assert_eq!(instance.drain_events(&mut bytes).unwrap(), 32);
    assert_eq!(
        i32::from_ne_bytes(bytes[..4].try_into().unwrap()),
        second_wd
    );
    instance.remove_watch(first_wd).unwrap();
    assert!(!first.watches_notifications(u32::MAX));
    assert!(second.watches_notifications(0x100));
}

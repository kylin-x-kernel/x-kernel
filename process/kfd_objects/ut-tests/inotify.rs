// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

/// Every watch stores `IN_UNMOUNT` as well: Linux ORs `FS_UNMOUNT` into each
/// mark unconditionally, so an assertion on a stored mask has to name it.
const UNMOUNT: u32 = linux_raw_sys::general::IN_UNMOUNT;

use super::*;

/// A distinct identity per path.
///
/// Watches are keyed by the object they name, so a test that registers the
/// same path twice must hand in the same identity for it to be one watch --
/// which is exactly what the kernel does with a resolved inode.
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

#[def_test]
fn add_watch_returns_ascending_descriptors() {
    let inotify = Inotify::new();
    let first = inotify.add_watch(IN_ALL_EVENTS, identity(b"/etc")).unwrap();
    let second = inotify.add_watch(IN_ALL_EVENTS, identity(b"/tmp")).unwrap();

    assert_eq!(first, FIRST_WATCH_DESCRIPTOR);
    assert_eq!(second, first + 1);
    assert_eq!(inotify.watch_count(), 2);
    assert_eq!(inotify.watch_identity(first), Some(identity(b"/etc")));
}

#[def_test]
fn descriptors_wrap_and_skip_occupied_numbers() {
    let inotify = Inotify::new();
    // The counter is private, which is what lets this test place it at the wrap
    // boundary directly: with the counter at `i32::MAX`, the next registration
    // wraps to the first number -- never `i32::MAX` itself, and never `-1`, the
    // overrun record's sentinel.
    inotify
        .next_watch_descriptor
        .store(i32::MAX, Ordering::Relaxed);
    let first = inotify
        .add_watch(IN_ALL_EVENTS, identity(b"/wrap-1"))
        .unwrap();
    assert_eq!(first, FIRST_WATCH_DESCRIPTOR);
    let second = inotify
        .add_watch(IN_ALL_EVENTS, identity(b"/wrap-2"))
        .unwrap();
    assert_eq!(second, FIRST_WATCH_DESCRIPTOR + 1);

    // An occupied number is skipped rather than handed out twice: put the
    // counter back on the first watch's descriptor -- which is still live -- and
    // the next registration walks past both occupied numbers.
    inotify.next_watch_descriptor.store(first, Ordering::Relaxed);
    let third = inotify
        .add_watch(IN_ALL_EVENTS, identity(b"/wrap-3"))
        .unwrap();
    assert_eq!(third, second + 1);
    assert_eq!(inotify.watch_count(), 3);
}

#[def_test]
fn only_event_bits_are_stored_for_matching() {
    let inotify = Inotify::new();

    // `IN_ISDIR` names an event *bit*, not a subscription to directories: Linux
    // keeps only `arg & IN_ALL_EVENTS` in the mark, so this watch subscribes to
    // no entry events at all -- the only thing its mask matches is the
    // `IN_UNMOUNT` every mark carries (asserted below).
    let none = inotify
        .add_watch(linux_raw_sys::general::IN_ISDIR, identity(b"/inotify-ut/isdir-only-marker"))
        .unwrap();
    assert_eq!(inotify.watch_mask(none), Some(UNMOUNT));

    // The behavioural bits outlive the call, but live beside the mask rather
    // than inside it -- otherwise `IN_ONESHOT` would sit in the mask an event is
    // matched against.
    let descriptor = inotify
        .add_watch(linux_raw_sys::general::IN_CREATE | IN_EXCL_UNLINK | IN_ONESHOT, identity(b"/inotify-ut/flags-marker"))
        .unwrap();
    assert_eq!(
        inotify.watch_mask(descriptor),
        Some(linux_raw_sys::general::IN_CREATE | UNMOUNT)
    );
    assert_eq!(inotify.watch_flags(descriptor), Some(IN_EXCL_UNLINK | IN_ONESHOT));

    // `IN_MASK_ADD` merges both halves.
    let again = inotify
        .add_watch(IN_MASK_ADD | linux_raw_sys::general::IN_DELETE, identity(b"/inotify-ut/flags-marker"))
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
    let inotify = Inotify::new();
    // Linux ORs `FS_UNMOUNT` into every mark, so a watch that names only
    // event bits still matches it.
    let descriptor = inotify
        .add_watch(linux_raw_sys::general::IN_CREATE, identity(b"/inotify-ut/unmount-marker"))
        .unwrap();
    assert_eq!(
        inotify.watch_mask(descriptor),
        Some(linux_raw_sys::general::IN_CREATE | UNMOUNT)
    );
}

#[def_test]
fn re_watching_an_identity_through_another_spelling_updates_one_watch() {
    let inotify = Inotify::new();
    let first = inotify.add_watch(0x2, identity(b"/etc")).unwrap();

    // The same object under a different spelling: Linux finds the mark on the
    // inode (`fsnotify_find_mark()`) and returns its descriptor with the mask
    // updated, rather than adding a second watch for the same object.
    let again = inotify.add_watch(0x4, identity(b"/etc")).unwrap();
    assert_eq!(first, again);
    assert_eq!(inotify.watch_count(), 1);
    assert_eq!(inotify.watch_mask(first), Some(0x4 | UNMOUNT));

    // A different object still gets its own watch.
    let other = inotify.add_watch(0x2, identity(b"/etc-other")).unwrap();
    assert_ne!(other, first);
    assert_eq!(inotify.watch_count(), 2);
}

#[def_test]
fn add_watch_replaces_mask_without_mask_add() {
    let inotify = Inotify::new();
    let descriptor = inotify.add_watch(IN_ALL_EVENTS, identity(b"/etc")).unwrap();
    let again = inotify.add_watch(0x2, identity(b"/etc")).unwrap();

    assert_eq!(descriptor, again);
    assert_eq!(inotify.watch_count(), 1);
    assert_eq!(inotify.watch_mask(descriptor), Some(0x2 | UNMOUNT));
}

#[def_test]
fn add_watch_with_mask_add_merges_bits() {
    let inotify = Inotify::new();
    let descriptor = inotify.add_watch(0x2, identity(b"/etc")).unwrap();
    let again = inotify.add_watch(IN_MASK_ADD | 0x4, identity(b"/etc")).unwrap();

    assert_eq!(descriptor, again);
    assert_eq!(inotify.watch_mask(descriptor), Some(0x6 | UNMOUNT));
}

#[def_test]
fn add_watch_with_mask_create_rejects_duplicate() {
    let inotify = Inotify::new();
    inotify.add_watch(IN_ALL_EVENTS, identity(b"/etc")).unwrap();

    assert_eq!(
        inotify.add_watch(IN_MASK_CREATE | IN_ALL_EVENTS, identity(b"/etc")),
        Err(KError::AlreadyExists)
    );
}

#[def_test]
fn add_watch_rejects_unknown_bits() {
    let inotify = Inotify::new();

    // The path argument is bounded by the adapter -- the same bytes Linux
    // bounds -- so the object itself checks only the mask domain.
    assert_eq!(
        inotify.add_watch(0x0001_0000, identity(b"/etc")),
        Err(KError::InvalidInput)
    );
    assert_eq!(inotify.add_watch(1 << 16, identity(b"/etc")), Err(KError::InvalidInput));
}

#[def_test]
fn add_watch_masks_are_confined_to_event_bits() {
    let inotify = Inotify::new();
    let descriptor = inotify.add_watch(IN_ALL_EVENTS, identity(b"/etc")).unwrap();

    assert_eq!(inotify.watch_mask(descriptor), Some(IN_ALL_EVENTS | UNMOUNT));
}

#[def_test]
fn add_watch_reports_storage_full_at_the_ceiling() {
    let inotify = Inotify::new();
    for index in 0..MAX_WATCHES {
        let path = alloc::format!("/w{index}");
        inotify.add_watch(IN_ALL_EVENTS, identity(path.as_bytes())).unwrap();
    }

    assert_eq!(
        inotify.add_watch(IN_ALL_EVENTS, identity(b"/overflow")),
        Err(KError::StorageFull)
    );
    assert_eq!(inotify.watch_count(), MAX_WATCHES);
}

#[def_test]
fn remove_watch_reports_missing_descriptor() {
    let inotify = Inotify::new();
    let descriptor = inotify.add_watch(IN_ALL_EVENTS, identity(b"/etc")).unwrap();

    assert_eq!(inotify.remove_watch(descriptor), Ok(()));
    assert_eq!(inotify.remove_watch(descriptor), Err(KError::InvalidInput));
    assert_eq!(inotify.watch_count(), 0);
}

#[def_test]
fn remove_watch_queues_in_ignored() {
    let inotify = Inotify::new();
    let descriptor = inotify.add_watch(IN_ALL_EVENTS, identity(b"/etc")).unwrap();
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
    let inotify = Inotify::new();
    inotify.add_watch(IN_ALL_EVENTS, identity(b"/etc")).unwrap();

    assert!(!inotify.poll().contains(IoEvents::IN));
    inotify.queue_event(FIRST_WATCH_DESCRIPTOR, 0x2, b"file");
    assert!(inotify.poll().contains(IoEvents::IN));
}

#[def_test]
fn queue_overflow_queues_the_overrun_record_immediately() {
    let inotify = Inotify::new();
    let fill = |inotify: &Arc<Inotify>| {
        for _ in 0..MAX_QUEUED_EVENTS {
            // Nameless records: each occupies exactly the 16-byte header.
            inotify.queue_event(1, 0x2, &[]);
        }
    };
    fill(&inotify);

    // Linux appends its pre-allocated overflow event the moment a push finds
    // the queue at its ceiling (`fsnotify_insert_event` jumps to `queue` with
    // `q_len++`), so the record takes a slot of its own rather than waiting for
    // a drain that a continuously busy producer never allows.
    inotify.queue_event(1, 0x2, b"dropped");
    assert_eq!(inotify.queued_bytes(), (MAX_QUEUED_EVENTS + 1) * 16);

    // Further overflowing pushes do not queue a second record: Linux queues the
    // overflow event only while it is not already queued.
    inotify.queue_event(1, 0x2, b"dropped-again");
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
    inotify.queue_event(1, 0x2, b"kept");
    assert_eq!(inotify.queued_bytes(), 32);
    fill(&inotify);
    inotify.queue_event(1, 0x2, b"dropped-again");
    // The ceiling is reached again, a fresh overrun record is queued, and the
    // 32-byte `kept` record is still counted with it.
    assert_eq!(inotify.queued_bytes(), (MAX_QUEUED_EVENTS + 1) * 16 + 16);
}

#[def_test]
fn drain_returns_the_copied_bytes_when_the_next_record_does_not_fit() {
    let inotify = Inotify::new();
    inotify.queue_event(1, 0x2, b"first");
    inotify.queue_event(1, 0x2, b"second");

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
    let inotify = Inotify::new();

    // Linux: "IN_MASK_ADD and IN_MASK_CREATE don't make sense together".
    assert_eq!(
        inotify.add_watch(IN_ALL_EVENTS | IN_MASK_ADD | IN_MASK_CREATE, identity(b"/etc")),
        Err(KError::InvalidInput)
    );
    assert_eq!(inotify.watch_count(), 0);
}

#[def_test]
fn drain_rejects_a_buffer_shorter_than_the_next_record() {
    let inotify = Inotify::new();
    inotify.queue_event(1, 0x2, b"a-long-name");

    let mut buf = [0u8; 16];
    assert_eq!(inotify.drain_events(&mut buf), Err(KError::InvalidInput));
    // "a-long-name" is 11 bytes: 12 rounded up to 16, so the record is 32.
    let mut enough = [0u8; 32];
    assert_eq!(inotify.drain_events(&mut enough).unwrap(), 32);
}

#[def_test]
fn drain_pads_names_to_the_event_header_boundary() {
    let inotify = Inotify::new();
    inotify.queue_event(1, 0x2, b"abc");

    // "abc" + NUL is 4 bytes, rounded up to the 16-byte event header: the
    // record is 16 + 16 = 32 bytes and `len` reports the rounded 16.
    let mut buf = [0u8; 32];
    assert_eq!(inotify.drain_events(&mut buf).unwrap(), 32);
    assert_eq!(u32::from_ne_bytes([buf[12], buf[13], buf[14], buf[15]]), 16);
    assert_eq!(&buf[16..20], b"abc\0");
    assert_eq!(&buf[20..32], &[0u8; 12]);
}

#[def_test]
fn empty_read_is_rejected() {
    let file = Inotify::new_file(0, kcred::initial_cred()).expect("inotify file opens");
    // `VfsFile::read_from` short-circuits an empty buffer in the VFS layer, so
    // call the file operations directly to reach the inotify guard.
    let fops = InotifyFops;
    let result = fops.read(&file, &mut [], 0);

    assert_eq!(result, Err(KError::InvalidInput));
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
    let file = Inotify::new_file(0, kcred::initial_cred()).expect("inotify file opens");
    let inotify = Inotify::from_file(&file).expect("inotify file exposes its state");
    let descriptor = inotify.add_watch(IN_ALL_EVENTS, identity(b"/etc")).unwrap();

    assert_eq!(inotify.watch_mask(descriptor), Some(IN_ALL_EVENTS | UNMOUNT));
}

#[def_test]
fn linux_mask_flags_are_accepted_and_stored() {
    let inotify = Inotify::new();
    // Linux accepts the behavioural flags; refusing them would fail calls
    // that work there. The stored mask keeps them, minus the add directives.
    let descriptor = inotify
        .add_watch(linux_raw_sys::general::IN_CREATE
                | linux_raw_sys::general::IN_ONLYDIR
                | linux_raw_sys::general::IN_DONT_FOLLOW, identity(b"/inotify-ut/mask-flags-marker"))
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
        inotify.add_watch(1 << 17, identity(b"/inotify-ut/mask-bad-marker")),
        Err(KError::InvalidInput)
    );
    assert_eq!(
        inotify.add_watch(1 << 20, identity(b"/inotify-ut/mask-bad-marker")),
        Err(KError::InvalidInput)
    );
}

#[def_test]
fn empty_mask_is_rejected_but_flags_alone_are_accepted() {
    let inotify = Inotify::new();
    // Linux: "the given event mask contains no valid events" is EINVAL; a
    // watch that subscribes to nothing must not consume the budget.
    assert_eq!(
        inotify.add_watch(0, identity(b"/inotify-ut/empty-mask-marker")),
        Err(KError::InvalidInput)
    );
    assert_eq!(inotify.watch_count(), 0);
    // Flags alone are accepted, matching Linux's "at least one valid bit"
    // (its check is against ALL_INOTIFY_BITS, not against the event bits).
    assert!(inotify
        .add_watch(linux_raw_sys::general::IN_ONLYDIR, identity(b"/inotify-ut/flags-only-marker"))
        .is_ok());
    assert_eq!(inotify.watch_count(), 1);
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
        inotify.queue_event(1, linux_raw_sys::general::IN_IGNORED, b"");
    }
    let mut buf = [0u8; 8192];
    let mut pos = 0;
    assert_eq!(file.read_from(&mut buf, &mut pos).unwrap(), 4096);
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{string::String, vec::Vec};
use klazy::Lazy;
use ksync::Mutex;
use unittest::def_test;

use super::*;

/// One captured `(device, inode, mask, name, cookie)` record.
type CapturedRecord = (u64, u64, u32, String, u32);

/// Records every notification, so assertions can filter by unique marker names
/// even though the tests run concurrently and each installs this same logger.
static LOG: Lazy<Mutex<Option<Vec<CapturedRecord>>>> = Lazy::new(|| Mutex::new(None));

fn capture_logger(device: u64, inode: u64, mask: u32, name: &[u8], cookie: u32) {
    let mut log = LOG.lock();
    let log = log.get_or_insert_with(Default::default);
    log.push((
        device,
        inode,
        mask,
        String::from_utf8_lossy(name).into_owned(),
        cookie,
    ));
}

fn records() -> Vec<CapturedRecord> {
    let log = LOG.lock();
    log.as_ref().cloned().unwrap_or_default()
}

/// Counts records matching one exact `(device, inode, mask, name)` tuple.
fn count_records(identity: (u64, u64), mask: u32, name: &str) -> usize {
    records()
        .into_iter()
        .filter(|(device, inode, m, n, _)| {
            (*device, *inode) == identity && *m == mask && n == name
        })
        .count()
}

#[def_test]
fn notify_rename_parts_pairs_the_two_halves_with_one_cookie() {
    set_entry_notifier(capture_logger);

    // The two halves name different objects (the entry's old and new parent)
    // and share one non-zero cookie.
    notify_rename_parts(
        Some(((8, 1001), alloc::string::String::from("notify-move-marker.txt"))),
        Some(((8, 1002), alloc::string::String::from("notify-move-marker.txt"))),
        false,
    );
    let captured: Vec<_> = records()
        .into_iter()
        .filter(|(_, _, _, name, _)| name == "notify-move-marker.txt")
        .collect();
    assert_eq!(captured.len(), 2);
    assert_eq!((captured[0].0, captured[0].1, captured[0].2), (8, 1001, IN_MOVED_FROM));
    assert_eq!((captured[1].0, captured[1].1, captured[1].2), (8, 1002, IN_MOVED_TO));
    // One cookie for both halves, and never 0 -- Linux's counter starts at 1.
    assert_eq!(captured[0].4, captured[1].4);
    assert_ne!(captured[0].4, 0);
    let first_cookie = captured[0].4;

    // A second rename gets a different cookie.
    notify_rename_parts(
        Some(((8, 1001), alloc::string::String::from("notify-move-a"))),
        Some(((8, 1001), alloc::string::String::from("notify-move-b"))),
        false,
    );
    let second: Vec<_> = records()
        .into_iter()
        .filter(|(_, _, _, name, _)| name == "notify-move-a" || name == "notify-move-b")
        .collect();
    assert_eq!(second.len(), 2);
    assert_ne!(second[0].4, first_cookie);

    // A moved directory carries IN_ISDIR on both halves.
    notify_rename_parts(
        Some(((8, 1001), alloc::string::String::from("notify-move-dir"))),
        Some(((8, 1001), alloc::string::String::from("notify-move-dir2"))),
        true,
    );
    let directory: Vec<_> = records()
        .into_iter()
        .filter(|(_, _, _, name, _)| name == "notify-move-dir" || name == "notify-move-dir2")
        .collect();
    assert_eq!(directory[0].2, IN_MOVED_FROM | IN_ISDIR);
    assert_eq!(directory[1].2, IN_MOVED_TO | IN_ISDIR);

    // A half that names no object is simply not reported.
    let before = records().len();
    notify_rename_parts(None, None, false);
    assert_eq!(records().len(), before);
    let _ = count_records((8, 1001), IN_MOVED_FROM, "notify-move-dir");
}

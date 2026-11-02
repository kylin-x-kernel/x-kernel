// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::string::String;

use ktask::KtaskRef;
use unittest::{assert_eq, def_test};

use super::{TaskStat, starttime_ticks};
use crate::AsThread;

#[def_test]
fn test_taskstat_display_default() {
    let stat = TaskStat::default();
    let text = alloc::format!("{stat}");
    let text = text.trim_end();
    assert!(text.starts_with("0 ("));
    assert!(text.ends_with(" 0"));
}

#[def_test]
fn test_taskstat_display_custom() {
    let stat = TaskStat {
        pid: 7,
        comm: "init".into(),
        state: 'R',
        priority: 10,
        nice: -10,
        ..Default::default()
    };
    let text = alloc::format!("{stat}");
    assert!(text.contains(" 10 -10 "));
}

#[def_test]
fn test_taskstat_display_places_vsize_and_signal_masks_in_linux_fields() {
    let stat = TaskStat {
        pid: 7,
        comm: "init".into(),
        state: 'S',
        vsize: Some(2_162_688),
        signal: 0x100,
        blocked: 0x200,
        sigignore: 0x400,
        sigcatch: 0x800,
        ..Default::default()
    };
    let text = alloc::format!("{stat}");
    let mut fields = text.split_whitespace();
    for _ in 0..22 {
        fields.next().unwrap();
    }
    assert_eq!(fields.next(), Some("2162688"));
    assert_eq!(fields.next(), Some("0"));
    for _ in 0..6 {
        fields.next().unwrap();
    }
    assert_eq!(fields.next(), Some("256"));
    assert_eq!(fields.next(), Some("512"));
    assert_eq!(fields.next(), Some("1024"));
    assert_eq!(fields.next(), Some("2048"));
}

#[def_test]
fn test_taskstat_display_masks_obsolete_signal_fields_like_linux() {
    let legacy_bits: u64 = (1 << 31) | (1 << 63) | 0b101;
    let stat = TaskStat {
        pid: 7,
        comm: "init".into(),
        signal: legacy_bits,
        blocked: legacy_bits,
        sigignore: legacy_bits,
        sigcatch: legacy_bits,
        ..Default::default()
    };
    let text = alloc::format!("{stat}");
    let mut fields = text.split_whitespace();
    for _ in 0..30 {
        fields.next().unwrap();
    }
    // Linux's legacy signal columns are `sig[0] & 0x7fffffff`, so the bits at
    // 31 and 63 are discarded while the low `0b101` survives in all four
    // columns.
    assert_eq!(fields.next(), Some("5"));
    assert_eq!(fields.next(), Some("5"));
    assert_eq!(fields.next(), Some("5"));
    assert_eq!(fields.next(), Some("5"));
}

#[def_test]
fn test_taskstat_display_prints_zero_vsize_without_address_space() {
    let stat = TaskStat {
        pid: 7,
        comm: "init".into(),
        ..Default::default()
    };
    let text = alloc::format!("{stat}");
    let mut fields = text.split_whitespace();
    for _ in 0..22 {
        fields.next().unwrap();
    }
    assert_eq!(
        fields.next(),
        Some("0"),
        "stat keeps the positional vsize field numeric when status omits VmSize"
    );
}

#[def_test]
fn test_starttime_ticks_conversion() {
    // USER_HZ=100: one tick per 10 ms.
    assert_eq!(starttime_ticks(0), 0);
    assert_eq!(starttime_ticks(9_999_999), 0);
    assert_eq!(starttime_ticks(10_000_000), 1);
    assert_eq!(starttime_ticks(118_490_000_000), 11_849);
}

#[def_test(user)]
fn test_taskstat_from_thread_reports_live_vma_total_and_tid() {
    let task = ktask::current();
    let thread = task.as_thread();
    let process = thread.process();
    let address_space = process
        .address_space()
        .expect("the current user task has a live address space");
    let expected = address_space
        .lock()
        .vmas()
        .map(|vma| vma.size() as u64)
        .sum::<u64>();
    assert!(expected > 0, "a live user task must have mapped VMAs");

    let stat = TaskStat::from_thread(&task);
    assert_eq!(stat.pid, thread.tid());
    assert_eq!(stat.vsize, Some(expected));
    assert_eq!(process.virtual_memory_size(), Some(expected));
}

/// Restores the task name even if an assertion panics.
struct NameGuard {
    task: KtaskRef,
    original: String,
}

impl NameGuard {
    fn new(task: &KtaskRef) -> Self {
        Self {
            task: task.clone(),
            original: task.name(),
        }
    }
}

impl Drop for NameGuard {
    fn drop(&mut self) {
        self.task.set_name(&self.original);
    }
}

#[def_test(user, serial)]
fn test_taskstat_from_thread_truncates_name_to_linux_length_and_utf8_boundary() {
    let task = ktask::current();
    let _restore = NameGuard::new(&task);

    task.set_name("aaaaaaaaaaaaaaaa");
    let ascii = TaskStat::from_thread(&task).comm;

    task.set_name("aaaaaaaaaaaaaaé");
    let multibyte = TaskStat::from_thread(&task).comm;

    // Linux stores at most 15 visible bytes plus the terminating NUL, and a
    // multibyte character must never be split at the truncation point. The
    // 14-byte prefix puts `é` across the 15-byte boundary, forcing the
    // implementation to back off rather than emit invalid UTF-8.
    assert_eq!(ascii, "aaaaaaaaaaaaaaa");
    assert_eq!(multibyte, "aaaaaaaaaaaaaa");
}

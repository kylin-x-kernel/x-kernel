// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{
    sync::Arc,
    vec,
    vec::Vec,
};
use core::{
    ffi::c_int,
    sync::atomic::{AtomicUsize, Ordering},
};

use anon_inodefs::AnonInodeFs;
use kpoll::IoEvents;
use ktime_types::SystemTime;
use kvfs::{DeviceId, FMode, FileOperations, OpenFlags, VfsFile, VfsResult};
use linux_raw_sys::general::stat;
use unittest::def_test;

use crate::{FdTable, Kstat};

struct SnapshotTestFops;

impl FileOperations for SnapshotTestFops {
    fn poll(&self, _file: &VfsFile) -> IoEvents {
        IoEvents::IN
    }
}

struct FlushCountFops {
    flushes: Arc<AtomicUsize>,
    result: VfsResult<()>,
}

impl FileOperations for FlushCountFops {
    fn flush(&self, _file: &VfsFile) -> VfsResult<()> {
        self.flushes.fetch_add(1, Ordering::Relaxed);
        self.result
    }
}

fn snapshot_test_file() -> alloc::sync::Arc<VfsFile> {
    AnonInodeFs::global()
        .get_file(
            "[snapshot-test]",
            alloc::sync::Arc::new(SnapshotTestFops),
            alloc::sync::Arc::new(()),
            FMode::READ,
            OpenFlags::NONBLOCK,
            kcred::initial_cred(),
        )
        .expect("snapshot test anon inode file opens")
}

fn flush_count_test_file(flushes: Arc<AtomicUsize>, result: VfsResult<()>) -> Arc<VfsFile> {
    AnonInodeFs::global()
        .get_file(
            "[fd-table-drop-test]",
            Arc::new(FlushCountFops { flushes, result }),
            Arc::new(()),
            FMode::READ,
            OpenFlags::empty(),
            kcred::initial_cred(),
        )
        .expect("fd table drop test file opens")
}

#[def_test(serial)]
fn last_binding_closes_descriptors_despite_retained_table_reference() {
    use kvfs::{FileLock, FileLockOwner, FileLockType};

    let flushes = Arc::new(AtomicUsize::new(0));
    let file = flush_count_test_file(flushes.clone(), Ok(()));
    let binding = crate::FdTableOwner::new_shared();
    let other_binding = binding.clone();
    let table = binding.reference();
    let fd = binding
        .table()
        .write()
        .add_file(16, file.clone(), false)
        .unwrap();
    let owner = table.read().lock_owner();
    file.set_lock(
        &FileLock::new(owner, 1, FileLockType::Write, 0, 0).unwrap(),
        false,
    )
    .unwrap();
    let query = FileLock::new(FileLockOwner::Posix(1), 2, FileLockType::Write, 0, 0).unwrap();

    drop(binding);
    assert_eq!(flushes.load(Ordering::Relaxed), 0);
    assert!(table.read().get_file(fd).is_ok());
    assert!(file.test_lock(&query).unwrap().is_some());

    drop(other_binding);
    assert_eq!(flushes.load(Ordering::Relaxed), 1);
    assert_eq!(table.read().count(), 0);
    assert!(file.test_lock(&query).unwrap().is_none());
    // Storage reclamation must not flush already closed descriptors again.
    drop(table);
    assert_eq!(flushes.load(Ordering::Relaxed), 1);
}

// Anonymous files share one inode, so these lock-lifecycle cases must not
// contend with each other through the singleton fixture.
#[def_test(serial)]
fn close_alias_removes_posix_locks_even_after_flush_error() {
    use kvfs::{FileLock, FileLockOwner, FileLockType};

    let flushes = Arc::new(AtomicUsize::new(0));
    let file = flush_count_test_file(flushes, Err(kerrno::KError::Io));
    let table = FdTable::new_shared();
    let (fd, alias, owner) = {
        let mut table = table.write();
        (
            table.add_file(16, file.clone(), false).unwrap(),
            table.add_file(16, file.clone(), false).unwrap(),
            table.lock_owner(),
        )
    };
    let lock = FileLock::new(owner, 1, FileLockType::Write, 0, 0).unwrap();
    file.set_lock(&lock, false).unwrap();
    let query = FileLock::new(FileLockOwner::Posix(1), 2, FileLockType::Write, 0, 0).unwrap();
    assert!(file.test_lock(&query).unwrap().is_some());
    let descriptor = table.write().file_close_fd_locked(alias).unwrap();
    assert_eq!(descriptor.close(owner), Err(kerrno::KError::Io));
    assert!(table.read().get_file(fd).is_ok());
    assert!(file.test_lock(&query).unwrap().is_none());
}

#[def_test(serial)]
fn copied_table_does_not_inherit_posix_owner_and_ofd_survives_close() {
    use kvfs::{FileLock, FileLockOwner, FileLockType};

    let file = snapshot_test_file();
    let table = FdTable::new_shared();
    let fd = table.write().add_file(16, file.clone(), false).unwrap();
    let copied = FdTable::clone_shared_from(&table);
    let owner = table.read().lock_owner();
    assert_ne!(owner, copied.read().lock_owner());
    file.set_lock(
        &FileLock::new(owner, 1, FileLockType::Write, 0, 10).unwrap(),
        false,
    )
    .unwrap();
    drop(copied);
    let query = FileLock::new(FileLockOwner::Posix(1), 2, FileLockType::Write, 0, 10).unwrap();
    assert!(file.test_lock(&query).unwrap().is_some());
    let ofd = FileLockOwner::ofd(&file);
    file.set_lock(
        &FileLock::new(ofd, 1, FileLockType::Write, 20, 10).unwrap(),
        false,
    )
    .unwrap();
    let inode = file.inode().clone();
    let observer = file
        .alloc_clone(FMode::READ, OpenFlags::empty(), Arc::new(SnapshotTestFops))
        .unwrap();
    let descriptor = table.write().file_close_fd_locked(fd).unwrap();
    descriptor.close(owner).unwrap();
    assert!(file.test_lock(&query).unwrap().is_none());
    let query = FileLock::new(FileLockOwner::Posix(1), 2, FileLockType::Write, 20, 10).unwrap();
    assert_eq!(observer.test_lock(&query).unwrap().unwrap().pid(), -1);
    drop(file);
    assert!(observer.test_lock(&query).unwrap().is_none());
    drop(inode);
}

#[def_test]
fn test_kstat_default() {
    let kstat = Kstat::default();
    assert_eq!(kstat.dev, 0);
    assert_eq!(kstat.ino, 1);
    assert_eq!(kstat.nlink, 1);
    assert_eq!(kstat.mode, 0);
    assert_eq!(kstat.uid, 1);
    assert_eq!(kstat.gid, 1);
    assert_eq!(kstat.size, 0);
    assert_eq!(kstat.blksize, 4096);
    assert_eq!(kstat.blocks, 0);
    assert_eq!(kstat.atime, SystemTime::UNIX_EPOCH);
    assert_eq!(kstat.mtime, SystemTime::UNIX_EPOCH);
    assert_eq!(kstat.ctime, SystemTime::UNIX_EPOCH);
}

#[def_test]
fn test_kstat_to_stat() {
    let kstat = Kstat {
        dev: 42,
        ino: 100,
        nlink: 3,
        mode: 0o755,
        uid: 1000,
        gid: 1000,
        size: 4096,
        blksize: 512,
        blocks: 8,
        rdev: DeviceId::default(),
        atime: SystemTime::from_unix_parts(1000, 500_000_000).unwrap(),
        mtime: SystemTime::from_unix_seconds(2000),
        ctime: SystemTime::from_unix_parts(3000, 123_456_789).unwrap(),
    };

    let s: stat = kstat.into();
    assert_eq!(s.st_dev, 42);
    assert_eq!(s.st_ino, 100);
    assert_eq!(s.st_nlink, 3);
    assert_eq!(s.st_mode, 0o755);
    assert_eq!(s.st_uid, 1000);
    assert_eq!(s.st_gid, 1000);
    assert_eq!(s.st_size, 4096);
    assert_eq!(s.st_blksize, 512);
    assert_eq!(s.st_blocks, 8);
    assert_eq!(s.st_atime, 1000);
    assert_eq!(s.st_atime_nsec, 500_000_000);
    assert_eq!(s.st_mtime, 2000);
    assert_eq!(s.st_mtime_nsec, 0);
    assert_eq!(s.st_ctime, 3000);
    assert_eq!(s.st_ctime_nsec, 123_456_789);
}

#[def_test]
fn test_fd_snapshot_keeps_file_alive_after_close() {
    let mut table = FdTable::default();
    let fd = table.add_file(16, snapshot_test_file(), true).unwrap();

    let snapshot = table.snapshot(fd).unwrap();
    table
        .file_close_fd_locked(fd)
        .unwrap()
        .close(table.lock_owner())
        .unwrap();

    assert_eq!(snapshot.fd(), fd);
    assert!(snapshot.cloexec());
    assert_eq!(snapshot.open_flags(), OpenFlags::NONBLOCK);
    assert_eq!(
        snapshot.path().display_path().unwrap(),
        "anon_inode:[snapshot-test]"
    );
    assert!(snapshot.file().poll().contains(IoEvents::IN));
    assert!(matches!(
        table.snapshot(fd),
        Err(kerrno::KError::BadFileDescriptor)
    ));
}

#[def_test]
fn test_duplicate_from_uses_lowest_free_slot_at_minimum() {
    let mut table = FdTable::default();
    let source = table.add_file(16, snapshot_test_file(), true).unwrap();
    assert_eq!(table.duplicate_from(source, 4, 16, false), Ok(4));
    assert_eq!(table.duplicate_from(source, 4, 16, false), Ok(5));
    table
        .file_close_fd_locked(4)
        .unwrap()
        .close(table.lock_owner())
        .unwrap();
    assert_eq!(table.duplicate_from(source, 4, 16, false), Ok(4));
    assert!(table.get(1).is_none());
    assert_eq!(table.count(), 3);
}

#[def_test]
fn test_duplicate_from_shares_file_but_sets_only_new_cloexec() {
    let mut table = FdTable::default();
    let file = snapshot_test_file();
    let source = table.add_file(16, file.clone(), true).unwrap();
    let plain = table.duplicate_from(source, 4, 16, false).unwrap();
    let cloexec = table.duplicate_from(source, 8, 16, true).unwrap();
    assert!(table.cloexec(source).unwrap());
    assert!(!table.cloexec(plain).unwrap());
    assert!(table.cloexec(cloexec).unwrap());
    assert!(Arc::ptr_eq(&file, &table.get_file(plain).unwrap()));
    table
        .file_close_fd_locked(source)
        .unwrap()
        .close(table.lock_owner())
        .unwrap();
    assert!(Arc::ptr_eq(&file, &table.get_file(cloexec).unwrap()));
    assert_eq!(table.get_file(plain).unwrap().flags(), OpenFlags::NONBLOCK);
}

#[def_test]
fn test_duplicate_from_rejects_invalid_minimum_without_mutation() {
    let mut table = FdTable::default();
    let source = table.add_file(16, snapshot_test_file(), false).unwrap();
    for minimum in [-1, 16, 17, c_int::MAX] {
        assert_eq!(
            table.duplicate_from(source, minimum, 16, true),
            Err(kerrno::KError::InvalidInput)
        );
    }
    assert_eq!(table.count(), 1);
    assert!(!table.cloexec(source).unwrap());
}

#[def_test]
fn test_duplicate_from_checks_source_before_minimum() {
    let mut table = FdTable::default();
    for source in [-1, 0, krlimit::FILE_LIMIT as c_int] {
        assert_eq!(
            table.duplicate_from(source, -1, 0, false),
            Err(kerrno::KError::BadFileDescriptor)
        );
    }
    assert_eq!(table.count(), 0);
}

#[def_test]
fn test_duplicate_from_exhausted_range_does_not_use_lower_holes() {
    let mut table = FdTable::default();
    let source = table.add_file(16, snapshot_test_file(), false).unwrap();
    assert_eq!(table.duplicate_from(source, 6, 8, false), Ok(6));
    assert_eq!(table.duplicate_from(source, 6, 8, false), Ok(7));
    assert_eq!(
        table.duplicate_from(source, 6, 8, true),
        Err(kerrno::KError::TooManyOpenFiles)
    );
    assert_eq!(table.count(), 3);
    assert!(!table.cloexec(6).unwrap());
    assert!(table.get(1).is_none());
    assert!(table.get(8).is_none());
}

#[def_test]
fn test_duplicate_from_lowered_limit_uses_low_hole_despite_high_fds() {
    let mut table = FdTable::default();
    let source = table.add_file(16, snapshot_test_file(), false).unwrap();
    for minimum in 8..12 {
        table.duplicate_from(source, minimum, 16, false).unwrap();
    }
    assert_eq!(table.count(), 5);
    assert_eq!(table.duplicate_from(source, 1, 4, false), Ok(1));
    assert_eq!(table.duplicate_from(8, 2, 4, false), Ok(2));
    assert!(table.get(12).is_none());
}

#[def_test]
fn test_open_and_duplicate_share_low_holes_after_limit_reduction() {
    let mut table = FdTable::default();
    let file = snapshot_test_file();
    let source = table.add_file(16, file.clone(), true).unwrap();
    for minimum in 8..12 {
        table.duplicate_from(source, minimum, 16, false).unwrap();
    }
    unittest::assert_eq!(table.count(), 5);

    unittest::assert_eq!(table.add_file(4, file.clone(), false), Ok(1));
    unittest::assert_eq!(table.duplicate_from(8, 0, 4, true), Ok(2));
    unittest::assert_eq!(table.add_file(4, file.clone(), true), Ok(3));
    unittest::assert_eq!(table.count(), 8);
    assert!(!table.cloexec(1).unwrap());
    assert!(table.cloexec(2).unwrap());
    assert!(table.cloexec(3).unwrap());
    assert!(Arc::ptr_eq(&file, &table.get_file(2).unwrap()));
    assert!(Arc::ptr_eq(&file, &table.get_file(8).unwrap()));

    unittest::assert_eq!(
        table.add_file(4, file, false),
        Err(kerrno::KError::TooManyOpenFiles)
    );
    unittest::assert_eq!(
        table.duplicate_from(8, 0, 4, false),
        Err(kerrno::KError::TooManyOpenFiles)
    );
    unittest::assert_eq!(table.count(), 8);
    assert!(table.get(4).is_none());
}

#[def_test]
fn test_duplicate_from_respects_physical_capacity_with_large_limit() {
    let mut table = FdTable::default();
    let source = table.add_file(16, snapshot_test_file(), false).unwrap();
    let last = (krlimit::FILE_LIMIT - 1) as c_int;
    assert_eq!(table.duplicate_from(source, last, u64::MAX, true), Ok(last));
    assert_eq!(
        table.duplicate_from(source, last, u64::MAX, false),
        Err(kerrno::KError::TooManyOpenFiles)
    );
    assert_eq!(
        table.duplicate_from(source, last + 1, u64::MAX, false),
        Err(kerrno::KError::TooManyOpenFiles)
    );
    assert_eq!(table.count(), 2);
    assert!(table.cloexec(last).unwrap());
}

#[def_test]
fn test_fd_table_drop_closes_remaining_descriptors() {
    let flushes = Arc::new(AtomicUsize::new(0));
    let mut table = FdTable::default();
    table
        .add_file(16, flush_count_test_file(flushes.clone(), Ok(())), false)
        .unwrap();

    drop(table);

    assert_eq!(flushes.load(Ordering::Relaxed), 1);
}

#[def_test]
fn test_fd_table_drop_continues_after_flush_error() {
    let failed_flushes = Arc::new(AtomicUsize::new(0));
    let successful_flushes = Arc::new(AtomicUsize::new(0));
    let mut table = FdTable::default();
    table
        .add_file(
            16,
            flush_count_test_file(failed_flushes.clone(), Err(kerrno::KError::Io)),
            false,
        )
        .unwrap();
    table
        .add_file(
            16,
            flush_count_test_file(successful_flushes.clone(), Ok(())),
            false,
        )
        .unwrap();

    drop(table);

    assert_eq!(failed_flushes.load(Ordering::Relaxed), 1);
    assert_eq!(successful_flushes.load(Ordering::Relaxed), 1);
}

/// Deterministic xorshift64* generator, so every differential run replays exactly.
struct OpRng(u64);

impl OpRng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// Returns a value below `bound`; callers never pass zero.
    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }
}

/// Reference model of occupancy and close-on-exec state.
///
/// Allocation is answered by brute force over the same permitted range, so a
/// mismatch localizes a defect in the bound, the search, or the command checks.
struct FdModel {
    occupied: Vec<bool>,
    cloexec: Vec<bool>,
}

impl FdModel {
    fn new() -> Self {
        Self {
            occupied: vec![false; krlimit::FILE_LIMIT],
            cloexec: vec![false; krlimit::FILE_LIMIT],
        }
    }

    fn lowest_free(&self, minimum: usize, max_nofile: u64) -> Option<usize> {
        let end = max_nofile.min(krlimit::FILE_LIMIT as u64) as usize;
        (minimum..end).find(|&fd| !self.occupied[fd])
    }

    fn assigned_ids(&self) -> Vec<usize> {
        (0..krlimit::FILE_LIMIT)
            .filter(|&fd| self.occupied[fd])
            .collect()
    }

    fn release(&mut self, fd: usize) {
        self.occupied[fd] = false;
        self.cloexec[fd] = false;
    }
}

/// The bound may only claim occupied slots: an over-estimate would skip holes.
fn assert_bound_is_valid(table: &FdTable) {
    let bound = table.test_next_free();
    assert!(bound <= krlimit::FILE_LIMIT, "bound {bound} exceeds capacity");
    for fd in 0..bound {
        assert!(table.get(fd).is_some(), "bound {bound} skips free fd {fd}");
    }
}

fn assert_matches_model(table: &FdTable, model: &FdModel, seed: u64, step: usize) {
    let actual: Vec<usize> = table.ids().collect();
    let expected = model.assigned_ids();
    assert_eq!(actual, expected, "occupancy seed={seed:#x} step={step}");
    assert_eq!(table.count(), expected.len(), "count seed={seed:#x} step={step}");
}

fn assert_cloexec_matches_model(table: &FdTable, model: &FdModel, seed: u64, step: usize) {
    for fd in 0..krlimit::FILE_LIMIT {
        if model.occupied[fd] {
            assert_eq!(
                table.cloexec(fd as c_int).ok(),
                Some(model.cloexec[fd]),
                "cloexec at {fd} seed={seed:#x} step={step}"
            );
        }
    }
}

#[def_test]
fn fd_ids_iterate_both_directions_across_bitmap_words() {
    let file = snapshot_test_file();
    let mut table = FdTable::default();
    let source = table.add_file(u64::MAX, file.clone(), false).unwrap();
    assert_eq!(source, 0);
    // Occupy the ids around every bitmap word edge plus the last physical slot.
    for fd in [1usize, 62, 63, 64, 65, 127, 128, 1022, 1023] {
        assert_eq!(
            table
                .duplicate_to(source, fd as c_int, false)
                .map(|(fd, _)| fd),
            Ok(fd as c_int)
        );
    }

    let expected = [0usize, 1, 62, 63, 64, 65, 127, 128, 1022, 1023];
    assert_eq!(table.count(), expected.len());
    assert_eq!(table.ids().collect::<Vec<_>>(), expected);
    assert_eq!(
        table.ids().rev().collect::<Vec<_>>(),
        expected.iter().rev().copied().collect::<Vec<_>>()
    );

    // Consuming from both ends must not repeat, skip, or reorder an id.
    let mut ids = table.ids();
    let mut order = Vec::new();
    while let Some(front) = ids.next() {
        order.push(front);
        let Some(back) = ids.next_back() else {
            break;
        };
        order.push(back);
    }
    assert_eq!(order, [0, 1023, 1, 1022, 62, 128, 63, 127, 64, 65]);
}

#[def_test]
fn test_next_free_bound_tracks_lowest_hole_and_range_searches() {
    let file = snapshot_test_file();
    let mut table = FdTable::default();
    for expected in 0..4 {
        assert_eq!(table.add_file(16, file.clone(), false), Ok(expected));
    }
    assert_eq!(table.test_next_free(), 4);

    // A released low slot lowers the bound and is allocated again first.
    table.file_close_fd_locked(2).unwrap();
    assert_eq!(table.test_next_free(), 2);
    assert_bound_is_valid(&table);
    assert_eq!(table.add_file(16, file.clone(), false), Ok(2));
    assert_eq!(table.test_next_free(), 3);

    // A fixed target above the bound must not absorb the holes below it.
    table.file_close_fd_locked(0).unwrap();
    assert_eq!(table.test_next_free(), 0);
    assert_eq!(table.duplicate_to(2, 8, false).map(|(fd, _)| fd), Ok(8));
    assert_eq!(table.test_next_free(), 0);
    assert_eq!(table.add_file(16, file.clone(), false), Ok(0));

    // A fixed target that is the bound itself moves the bound past it.
    assert_eq!(table.test_next_free(), 1);
    assert_eq!(table.duplicate_to(2, 1, false).map(|(fd, _)| fd), Ok(1));
    assert_eq!(table.test_next_free(), 2);

    // A range search above the bound must not claim the holes below it either.
    let mut table = FdTable::default();
    assert_eq!(table.add_file(16, file.clone(), false), Ok(0));
    assert_eq!(table.add_file(16, file.clone(), false), Ok(1));
    table.file_close_fd_locked(0).unwrap();
    assert_eq!(table.duplicate_from(1, 700, 16, false), Err(kerrno::KError::InvalidInput));
    assert_eq!(table.duplicate_from(1, 700, u64::MAX, false), Ok(700));
    assert_eq!(table.test_next_free(), 0);
    assert_bound_is_valid(&table);
    assert_eq!(table.add_file(16, file.clone(), false), Ok(0));
}

#[def_test]
fn test_copied_table_inherits_the_free_descriptor_bound() {
    let file = snapshot_test_file();
    let shared = FdTable::new_shared();
    {
        let mut table = shared.write();
        for expected in 0..3 {
            assert_eq!(table.add_file(16, file.clone(), false), Ok(expected));
        }
        table.file_close_fd_locked(0).unwrap();
        assert_eq!(table.test_next_free(), 0);
    }

    let copied_shared = FdTable::clone_shared_from(&shared);
    {
        let copied = copied_shared.read();
        // An under-estimate would stay correct but slow; an over-estimate would
        // return 3 instead of the freed slot 0.
        assert_eq!(copied.test_next_free(), 0);
        assert_bound_is_valid(&copied);
    }

    let mut copied = copied_shared.write();
    assert_eq!(copied.add_file(16, file.clone(), false), Ok(0));
    assert_eq!(copied.add_file(16, file.clone(), false), Ok(3));
    assert_bound_is_valid(&copied);
}

#[def_test]
fn fd_table_matches_reference_model_under_random_operations() {
    const FILE_LIMIT: usize = krlimit::FILE_LIMIT;

    for seed in [1u64, 0x5eed, 0xdead_beef] {
        let mut rng = OpRng(seed);
        let mut table = FdTable::default();
        let mut model = FdModel::new();
        let file = snapshot_test_file();

        for step in 0..2000 {
            // Mix finite and infinite soft limits so bounded and unbounded
            // searches are both exercised.
            let soft = match rng.below(6) {
                0 => FILE_LIMIT as u64,
                1 => u64::MAX,
                _ => rng.below(FILE_LIMIT + 1) as u64,
            };
            let random_fd = |rng: &mut OpRng| match rng.below(8) {
                0 => -1,
                _ => rng.below(FILE_LIMIT) as c_int,
            };

            match rng.below(13) {
                0..=3 => {
                    // open(): lowest free descriptor below the soft limit.
                    let cloexec = rng.next() & 1 == 1;
                    let expected = match model.lowest_free(0, soft) {
                        Some(fd) => {
                            model.occupied[fd] = true;
                            model.cloexec[fd] = cloexec;
                            Some(fd as c_int)
                        }
                        None => None,
                    };
                    assert_eq!(
                        table.add_file(soft, file.clone(), cloexec).ok(),
                        expected,
                        "open seed={seed:#x} step={step}"
                    );
                }
                4..=5 => {
                    // F_DUPFD: lowest free descriptor at or above the minimum.
                    let old_fd = random_fd(&mut rng);
                    let minimum = match rng.below(8) {
                        0 => FILE_LIMIT as c_int + 1,
                        _ => rng.below(FILE_LIMIT + 1) as c_int,
                    };
                    let cloexec = rng.next() & 1 == 1;
                    let expected = if old_fd < 0 || !model.occupied[old_fd as usize] {
                        Err(kerrno::KError::BadFileDescriptor)
                    } else if minimum as u64 >= soft {
                        Err(kerrno::KError::InvalidInput)
                    } else {
                        match model.lowest_free(minimum as usize, soft) {
                            Some(fd) => {
                                model.occupied[fd] = true;
                                model.cloexec[fd] = cloexec;
                                Ok(fd as c_int)
                            }
                            None => Err(kerrno::KError::TooManyOpenFiles),
                        }
                    };
                    assert_eq!(
                        table.duplicate_from(old_fd, minimum, soft, cloexec),
                        expected,
                        "dup_min seed={seed:#x} step={step}"
                    );
                }
                6 => {
                    // insert_file() ignores the soft limit but not capacity.
                    let cloexec = rng.next() & 1 == 1;
                    let expected = match model.lowest_free(0, FILE_LIMIT as u64) {
                        Some(fd) => {
                            model.occupied[fd] = true;
                            model.cloexec[fd] = cloexec;
                            Some(fd)
                        }
                        None => None,
                    };
                    assert_eq!(
                        table.insert_file(file.clone(), cloexec).ok(),
                        expected,
                        "insert seed={seed:#x} step={step}"
                    );
                }
                7..=8 => {
                    // dup2/dup3: fixed target, replaced entry reported back.
                    let old_fd = random_fd(&mut rng);
                    let new_fd = match rng.below(8) {
                        0 => FILE_LIMIT as c_int,
                        1 => -1,
                        _ => rng.below(FILE_LIMIT) as c_int,
                    };
                    let cloexec = rng.next() & 1 == 1;
                    let expected = if new_fd < 0
                        || new_fd as usize >= FILE_LIMIT
                        || old_fd < 0
                        || !model.occupied[old_fd as usize]
                    {
                        Err(kerrno::KError::BadFileDescriptor)
                    } else if old_fd == new_fd {
                        Ok((new_fd, None))
                    } else {
                        let replaced = if model.occupied[new_fd as usize] {
                            Some(model.cloexec[new_fd as usize])
                        } else {
                            None
                        };
                        model.occupied[new_fd as usize] = true;
                        model.cloexec[new_fd as usize] = cloexec;
                        Ok((new_fd, replaced))
                    };
                    let actual = table
                        .duplicate_to(old_fd, new_fd, cloexec)
                        .map(|(fd, replaced)| (fd, replaced.map(|entry| entry.cloexec())));
                    assert_eq!(actual, expected, "dup2 seed={seed:#x} step={step}");
                }
                9 => {
                    let fd = random_fd(&mut rng);
                    let expected = if fd < 0 || !model.occupied[fd as usize] {
                        Err(kerrno::KError::BadFileDescriptor)
                    } else {
                        let cloexec = model.cloexec[fd as usize];
                        model.release(fd as usize);
                        Ok(cloexec)
                    };
                    assert_eq!(
                        table.file_close_fd_locked(fd).map(|entry| entry.cloexec()),
                        expected,
                        "close seed={seed:#x} step={step}"
                    );
                }
                10 => {
                    let first = rng.below(FILE_LIMIT) as c_int;
                    let last = rng.below(FILE_LIMIT) as c_int;
                    let expected = (first..=last)
                        .filter(|&fd| model.occupied[fd as usize])
                        .count();
                    assert_eq!(
                        table.remove_range(first, last).len(),
                        expected,
                        "remove_range seed={seed:#x} step={step}"
                    );
                    for fd in first..=last {
                        model.release(fd as usize);
                    }
                }
                11 => {
                    let fd = random_fd(&mut rng);
                    let value = rng.next() & 1 == 1;
                    let expected = if fd < 0 || !model.occupied[fd as usize] {
                        Err(kerrno::KError::BadFileDescriptor)
                    } else {
                        model.cloexec[fd as usize] = value;
                        Ok(())
                    };
                    assert_eq!(
                        table.set_cloexec(fd, value),
                        expected,
                        "set_cloexec seed={seed:#x} step={step}"
                    );
                }
                _ => {
                    let expected = (0..FILE_LIMIT)
                        .filter(|&fd| model.occupied[fd] && model.cloexec[fd])
                        .count();
                    assert_eq!(
                        table.remove_cloexec_files().len(),
                        expected,
                        "remove_cloexec seed={seed:#x} step={step}"
                    );
                    for fd in 0..FILE_LIMIT {
                        if model.cloexec[fd] {
                            model.release(fd);
                        }
                    }
                }
            }

            assert_bound_is_valid(&table);
            assert_matches_model(&table, &model, seed, step);
            if step % 128 == 0 {
                assert_cloexec_matches_model(&table, &model, seed, step);
                let reversed: Vec<usize> = table.ids().rev().collect();
                let expected = model.assigned_ids();
                assert_eq!(
                    reversed,
                    expected.iter().rev().copied().collect::<Vec<_>>(),
                    "reverse ids seed={seed:#x} step={step}"
                );
            }
        }
    }
}

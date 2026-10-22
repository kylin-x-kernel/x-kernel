// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use khal::paging::MappingFlags;
use kprocess::AsThread;
use kvfs::SeqIterator;
use unittest::{assert, assert_eq, def_test};

use super::{
    MapsEntry, PROC_MAPS_ADDR_WIDTH, PROC_MAPS_PATH_COLUMN, ProcNsKind, format_oom_score_adj,
    format_task_status, maps_permissions, parse_oom_score_adj_input, render_maps_line,
    special_mapping_name,
};

fn task_directory_path(process: alloc::sync::Weak<kprocess::Process>) -> kvfs::Path {
    static FS_TYPE: kvfs::FileSystemType = kvfs::FileSystemType::internal("proc-task-test");
    let super_block = kvfs::SimpleFs::new_with(&FS_TYPE, 0, move |fs| {
        let ops = alloc::sync::Arc::new(super::ProcessTaskDir {
            fs: fs.clone(),
            process,
        });
        kvfs::SimpleDir::new_maker(fs, ops)
    });
    kvfs::Mount::new_root(&super_block).root_path()
}

#[def_test(user)]
fn test_task_directory_link_count_uses_its_live_target() {
    // This user-test fixture owns its process and starts no extra threads.
    let task = ktask::current();
    let process = task.as_thread().process();
    let path = task_directory_path(alloc::sync::Arc::downgrade(process));
    // start_user_task publishes this exact fixture thread before running it.
    assert!(process.threads().contains(&task.as_thread().tid()));
    let count = kprocess::procfs::thread_group_member_count(process);
    assert_eq!(path.getattr().unwrap().nlink, 2 + count as u64);
    assert_eq!(path.metadata().nlink, 2);
}

#[def_test(user)]
fn test_task_directory_link_count_after_target_is_dropped() {
    // The user fixture has already initialized global init. This separate
    // parentless identity publishes no task or PID slot and has one owner.
    let process = kprocess::Process::new_init(0x7fff_ff01);
    let weak = alloc::sync::Arc::downgrade(&process);
    let path = task_directory_path(weak.clone());
    assert_eq!(alloc::sync::Arc::strong_count(&process), 1);
    drop(process);
    assert!(weak.upgrade().is_none());
    assert_eq!(path.getattr().unwrap().nlink, 2);
    assert_eq!(path.metadata().nlink, 2);
}

#[def_test(user)]
fn test_maps_rewind_observes_captured_image_heap_updates() {
    let task = ktask::current();
    let process = task.as_thread().process();
    let old_heap_top = process.heap_top().unwrap();
    let mut maps = super::MapsIter::new(&task).unwrap();
    let start = memaddr::VirtAddr::from_usize(super::USER_HEAP_BASE);
    let end = super::USER_HEAP_BASE + memaddr::PAGE_SIZE_4K;
    maps.aspace
        .lock()
        .map(
            start,
            memaddr::PAGE_SIZE_4K,
            MappingFlags::READ | MappingFlags::WRITE,
            false,
            memspace::VmRuntimeRef::new_anon_private(start, khal::paging::PageSize::Size4K),
        )
        .unwrap();
    process.set_heap_top(end).unwrap();
    maps.rewind();
    let entry = maps
        .entries
        .iter()
        .find(|entry| entry.start == start.as_usize())
        .unwrap();
    assert_eq!(entry.name, Some("[heap]"));

    process.set_heap_top(super::USER_HEAP_BASE).unwrap();
    maps.rewind();
    let entry = maps
        .entries
        .iter()
        .find(|entry| entry.start == start.as_usize())
        .unwrap();
    assert_eq!(entry.name, None);
    maps.aspace
        .lock()
        .unmap(start, memaddr::PAGE_SIZE_4K)
        .unwrap();
    process.set_heap_top(old_heap_top).unwrap();
}

#[def_test]
fn test_namespace_kind_names_cover_linux_set() {
    assert_eq!(ProcNsKind::ALL.len(), 8);
    for name in [
        "mnt",
        "pid",
        "pid_for_children",
        "net",
        "ipc",
        "uts",
        "user",
        "cgroup",
    ] {
        assert_eq!(
            ProcNsKind::from_name(name).map(ProcNsKind::name),
            Some(name),
            "every Linux ns type must round-trip"
        );
    }
    assert!(ProcNsKind::from_name("unknown").is_none());
}

#[def_test]
fn test_task_status_includes_process_name() {
    let status = format_task_status("test-bin", 12, 34, &kcred::Cred::root());

    assert!(status.contains("Name:\ttest-bin\n"));
    assert!(status.contains("Tgid:\t12\n"));
    assert!(status.contains("Pid:\t34\n"));
}

// These fixtures publish targets but never activate them. Reader credentials
// are restored before a target is inspected, including every error return.
struct ScopedStatusReaderCred {
    reader: kprocess::CurrentThread,
    original: alloc::sync::Arc<kcred::Cred>,
}

impl ScopedStatusReaderCred {
    fn install(next: kcred::Cred) -> Self {
        let reader = kprocess::current_user_thread();
        let original = reader.real_cred();
        core::assert!(alloc::sync::Arc::ptr_eq(&original, &reader.cred()));
        let guard = Self { reader, original };
        guard.reader.commit_creds(next);
        guard
    }
}

impl Drop for ScopedStatusReaderCred {
    fn drop(&mut self) {
        self.reader.commit_creds(self.original.prepare());
    }
}

fn status_credential_target(
    cred: kcred::Cred,
    is_sibling: bool,
) -> kvfs::VfsResult<kprocess::PublishedUserTask> {
    let prepared = {
        let _restore = ScopedStatusReaderCred::install(cred);
        let reader = kprocess::current_user_thread();
        if is_sibling {
            reader.prepare_thread_clone()?
        } else {
            reader.prepare_process_fork(kprocess::ProcessForkConfig {
                parent: kprocess::ForkParent::Caller,
                address_space: kprocess::ForkAddressSpace::Shared,
                fs: kprocess::ForkFs::Shared,
                signal_actions: kprocess::ForkSignalActions::Shared,
                fd_table: kprocess::ForkFdTable::Shared,
                namespace_flags: Default::default(),
                exit_signal: None,
            })?
        }
    };
    let (thread, identity) = prepared.into_parts();
    kprocess::publish_user_task(ktask::TaskInner::new_user(
        || {},
        alloc::string::String::from("procfs-status-credential-target"),
        identity,
        thread,
    ))
}

fn status_id_columns(status: &str, key: &str) -> Option<[u32; 4]> {
    let mut rows = status.lines().filter_map(|line| line.strip_prefix(key));
    let row = rows.next()?;
    if rows.next().is_some() {
        return None;
    }
    let mut columns = row.split_whitespace();
    let ids = [
        columns.next()?.parse().ok()?,
        columns.next()?.parse().ok()?,
        columns.next()?.parse().ok()?,
        columns.next()?.parse().ok()?,
    ];
    if columns.next().is_some() {
        return None;
    }
    Some(ids)
}

fn status_distinct_ids() -> kcred::Cred {
    let mut cred = kcred::Cred::root();
    cred.set_resgid(Some(201), Some(202), Some(203)).unwrap();
    cred.set_fsgid(204);
    // Keeping effective UID zero permits a fourth, distinct filesystem UID.
    cred.set_resuid(Some(101), Some(0), Some(103)).unwrap();
    cred.set_fsuid(104);
    cred
}

static STATUS_TEST_FS_TYPE: kvfs::FileSystemType =
    kvfs::FileSystemType::internal("procfs-status-credential-test");

fn status_test_root() -> kvfs::Path {
    let fs = kvfs::SimpleFs::new_with(&STATUS_TEST_FS_TYPE, 0x9fa0, |fs| {
        kvfs::SimpleDir::new_maker(
            fs.clone(),
            alloc::sync::Arc::new(super::ProcFsHandler::new(fs)),
        )
    });
    kvfs::Mount::new_root(&fs).root_path()
}

fn open_status(
    root: &kvfs::Path,
    name: alloc::string::String,
) -> kvfs::VfsResult<alloc::sync::Arc<kvfs::VfsFile>> {
    kvfs::Filename::new(name).open_with_flags_at(
        root,
        root,
        0,
        kvfs::NodePermission::empty(),
        kvfs::NodePermission::empty(),
        kcred::initial_cred(),
    )
}

fn read_status(file: &kvfs::VfsFile) -> kvfs::VfsResult<alloc::string::String> {
    let mut bytes = alloc::vec::Vec::new();
    let mut chunk = [0u8; 31];
    for _ in 0..128 {
        let count = file.read(&mut chunk)?;
        if count == 0 {
            return alloc::string::String::from_utf8(bytes)
                .map_err(|_| kvfs::VfsError::InvalidData);
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    Err(kvfs::VfsError::BadState)
}

#[def_test(user, serial)]
fn test_status_root_credential_columns_remain_zero() {
    let _reader = ScopedStatusReaderCred::install(kcred::Cred::new(71, 81));
    let target = status_credential_target(kcred::Cred::root(), false).unwrap();
    let status = super::task_status(target.task());
    assert_eq!(status_id_columns(&status, "Uid:"), Some([0; 4]));
    assert_eq!(status_id_columns(&status, "Gid:"), Some([0; 4]));
}

#[def_test(user, serial)]
fn test_status_nonzero_target_credentials_are_not_reader_credentials() {
    let _reader = ScopedStatusReaderCred::install(kcred::Cred::new(71, 81));
    let target = status_credential_target(kcred::Cred::new(1234, 4321), false).unwrap();
    let reader = ktask::current();
    assert!(!ktask::KtaskRef::ptr_eq(target.task(), &reader));
    assert!(!alloc::sync::Arc::ptr_eq(
        target.task().as_thread().process(),
        reader.as_thread().process()
    ));
    assert_eq!(kprocess::current_real_cred().ruid(), 71);
    assert_eq!(kprocess::current_real_cred().rgid(), 81);
    let status = super::task_status(target.task());
    assert_eq!(status_id_columns(&status, "Uid:"), Some([1234; 4]));
    assert_eq!(status_id_columns(&status, "Gid:"), Some([4321; 4]));
}

#[def_test(user, serial)]
fn test_status_orders_real_effective_saved_filesystem_ids() {
    let _reader = ScopedStatusReaderCred::install(kcred::Cred::new(71, 81));
    let target = status_credential_target(status_distinct_ids(), false).unwrap();
    let cred = target.task().as_thread().real_cred();
    assert_eq!(
        [cred.ruid(), cred.euid(), cred.suid(), cred.fsuid()],
        [101, 0, 103, 104]
    );
    assert_eq!(
        [cred.rgid(), cred.egid(), cred.sgid(), cred.fsgid()],
        [201, 202, 203, 204]
    );
    let status = super::task_status(target.task());
    assert_eq!(status_id_columns(&status, "Uid:"), Some([101, 0, 103, 104]));
    assert_eq!(
        status_id_columns(&status, "Gid:"),
        Some([201, 202, 203, 204])
    );
}

#[def_test(user, serial)]
fn test_status_same_process_threads_keep_their_own_credentials() {
    let _reader = ScopedStatusReaderCred::install(kcred::Cred::new(71, 81));
    let first = status_credential_target(kcred::Cred::new(1234, 4321), true).unwrap();
    let second = status_credential_target(kcred::Cred::new(2345, 5432), true).unwrap();
    let reader = ktask::current();
    let owner = reader.as_thread().process();
    assert!(alloc::sync::Arc::ptr_eq(
        first.task().as_thread().process(),
        owner
    ));
    assert!(alloc::sync::Arc::ptr_eq(
        second.task().as_thread().process(),
        owner
    ));
    assert!(first.task().as_thread().tid() != second.task().as_thread().tid());
    let first_status = super::task_status(first.task());
    let second_status = super::task_status(second.task());
    assert_eq!(status_id_columns(&first_status, "Uid:"), Some([1234; 4]));
    assert_eq!(status_id_columns(&first_status, "Gid:"), Some([4321; 4]));
    assert_eq!(status_id_columns(&second_status, "Uid:"), Some([2345; 4]));
    assert_eq!(status_id_columns(&second_status, "Gid:"), Some([5432; 4]));
}

#[def_test(user, serial)]
fn test_status_reader_credential_change_does_not_retarget_snapshot() {
    let _reader = ScopedStatusReaderCred::install(kcred::Cred::new(71, 81));
    let target = status_credential_target(kcred::Cred::new(1234, 4321), false).unwrap();
    let before = super::task_status(target.task());
    {
        let _changed = ScopedStatusReaderCred::install(kcred::Cred::new(98, 89));
        assert_eq!(kprocess::current_real_cred().ruid(), 98);
        let changed = super::task_status(target.task());
        assert_eq!(status_id_columns(&changed, "Uid:"), Some([1234; 4]));
        assert_eq!(status_id_columns(&changed, "Gid:"), Some([4321; 4]));
    }
    assert_eq!(kprocess::current_real_cred().ruid(), 71);
    assert_eq!(kprocess::current_real_cred().rgid(), 81);
    let after = super::task_status(target.task());
    assert_eq!(
        status_id_columns(&before, "Uid:"),
        status_id_columns(&after, "Uid:")
    );
    assert_eq!(
        status_id_columns(&before, "Gid:"),
        status_id_columns(&after, "Gid:")
    );
}

#[def_test(user, serial)]
fn test_status_vfs_process_and_thread_paths_render_selected_credentials() {
    let _reader = ScopedStatusReaderCred::install(kcred::Cred::new(71, 81));
    let target = status_credential_target(status_distinct_ids(), false).unwrap();
    let root = status_test_root();
    let thread = target.task().as_thread();
    for name in [
        alloc::format!("/{}/status", thread.process().pid()),
        alloc::format!("/{}/task/{}/status", thread.process().pid(), thread.tid()),
    ] {
        let file = open_status(&root, name).unwrap();
        let text = read_status(&file).unwrap();
        assert_eq!(status_id_columns(&text, "Uid:"), Some([101, 0, 103, 104]));
        assert_eq!(status_id_columns(&text, "Gid:"), Some([201, 202, 203, 204]));
        assert!(text.contains(&alloc::format!("Pid:\t{}\n", thread.tid())));
    }
    assert_eq!(kprocess::current_real_cred().ruid(), 71);
}

#[def_test(user, serial)]
fn test_status_partial_reads_keep_one_record_and_rewind_refreshes_credentials() {
    let _reader = ScopedStatusReaderCred::install(kcred::Cred::new(7, 8));
    let target = ktask::current();
    let root = status_test_root();
    let pid = target.as_thread().process().pid();
    let name = alloc::format!("/{pid}/status");
    let first = open_status(&root, name.clone()).unwrap();
    let not_yet_read = open_status(&root, name).unwrap();
    let original = super::task_status(&target);
    let split = original.find("Gid:").unwrap();
    let mut prefix = alloc::vec![0u8; split];
    assert_eq!(first.read(&mut prefix).unwrap(), split);

    // Change both contents and width after the first record has started.
    let _changed = ScopedStatusReaderCred::install(kcred::Cred::new(1234567890, 987654321));
    let mut observed = alloc::string::String::from_utf8(prefix).unwrap();
    observed.push_str(&read_status(&first).unwrap());
    assert_eq!(status_id_columns(&observed, "Uid:"), Some([7; 4]));
    assert_eq!(status_id_columns(&observed, "Gid:"), Some([8; 4]));

    // Each open has its own read state; contents are not frozen at open.
    let newer = read_status(&not_yet_read).unwrap();
    assert_eq!(status_id_columns(&newer, "Uid:"), Some([1234567890; 4]));
    assert_eq!(status_id_columns(&newer, "Gid:"), Some([987654321; 4]));

    assert_eq!(first.llseek(0, 0).unwrap(), 0);
    let rewound = read_status(&first).unwrap();
    assert_eq!(status_id_columns(&rewound, "Uid:"), Some([1234567890; 4]));
    assert_eq!(status_id_columns(&rewound, "Gid:"), Some([987654321; 4]));
}

#[def_test(user, serial)]
fn test_status_collected_target_keeps_esrch_without_reader_fallback() {
    let _reader = ScopedStatusReaderCred::install(kcred::Cred::new(71, 81));
    let target = status_credential_target(kcred::Cred::new(1234, 4321), false).unwrap();
    let weak = ktask::KtaskRef::downgrade(target.task());
    let root = status_test_root();
    let pid = target.task().as_thread().process().pid();
    let file = open_status(&root, alloc::format!("/{pid}/status")).unwrap();
    target.abort();
    assert!(matches!(
        super::upgrade_task(&weak),
        Err(kvfs::VfsError::NoSuchProcess)
    ));
    assert_eq!(
        file.read(&mut [0u8; 31]),
        Err(kvfs::VfsError::NoSuchProcess)
    );
    assert_eq!(kprocess::current_real_cred().ruid(), 71);
}

#[def_test(user, serial)]
fn test_status_retained_target_preserves_credentials_after_abort() {
    let _reader = ScopedStatusReaderCred::install(kcred::Cred::new(71, 81));
    let target = status_credential_target(kcred::Cred::new(1234, 4321), false).unwrap();
    let retained = target.task().clone();
    let weak = ktask::KtaskRef::downgrade(&retained);
    target.abort();
    let status = super::task_status(&retained);
    assert_eq!(status_id_columns(&status, "Uid:"), Some([1234; 4]));
    assert_eq!(status_id_columns(&status, "Gid:"), Some([4321; 4]));
    drop(retained);
    assert!(matches!(
        super::upgrade_task(&weak),
        Err(kvfs::VfsError::NoSuchProcess)
    ));
}

#[def_test]
fn test_format_oom_score_adj_appends_newline() {
    assert_eq!(format_oom_score_adj(200), b"200\n");
}

#[def_test]
fn test_parse_oom_score_adj_accepts_newline_terminated_input() {
    assert_eq!(parse_oom_score_adj_input(b"100\n").unwrap(), Some(100));
}

#[def_test]
fn test_parse_oom_score_adj_accepts_empty_input_as_noop() {
    assert_eq!(parse_oom_score_adj_input(b"").unwrap(), None);
}

#[def_test]
fn test_parse_oom_score_adj_rejects_invalid_input() {
    assert!(parse_oom_score_adj_input(b"abc\n").is_err());
}

#[def_test]
fn test_maps_permissions_formats_shared_exec() {
    assert_eq!(
        maps_permissions(MappingFlags::READ | MappingFlags::EXECUTE | MappingFlags::SHARED),
        *b"r-xs"
    );
}

#[def_test]
fn test_special_mapping_name_recognizes_heap_and_stack() {
    assert_eq!(
        special_mapping_name(
            super::USER_HEAP_BASE,
            super::USER_HEAP_BASE + 0x1000,
            super::USER_HEAP_BASE + 0x2000
        ),
        Some("[heap]")
    );
    assert_eq!(
        special_mapping_name(
            super::USER_STACK_TOP - 0x1000,
            super::USER_STACK_TOP,
            super::USER_HEAP_BASE
        ),
        Some("[stack]")
    );
}

#[def_test]
fn test_render_maps_line_aligns_path_column() {
    let line = render_maps_line(&MapsEntry {
        start: 0x4000_0000,
        end: 0x4000_1000,
        flags: MappingFlags::READ | MappingFlags::WRITE,
        offset: 0,
        inode: 42,
        path: Some("/bin/test".into()),
        name: None,
    });

    assert_eq!(&line[..PROC_MAPS_ADDR_WIDTH], "000040000000");
    assert_eq!(line.chars().nth(PROC_MAPS_PATH_COLUMN), Some('/'));
}

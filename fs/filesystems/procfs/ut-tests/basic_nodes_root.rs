// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{format, string::String, sync::Arc};

use kvfs::{
    DirMapping, FileSystemType, Filename, Mount, NodePermission, OpenFlags, SimpleDir, SimpleFs,
    VfsError,
};
use unittest::{assert, assert_eq, def_test};

use super::{add_root_entries, add_sys_entries};

static TEST_FILE_SYSTEM_TYPE: FileSystemType = FileSystemType::internal("procfs-printk-test");

fn build_basic_root(fs: Arc<SimpleFs>) -> kvfs::DirMaker {
    let mut root = DirMapping::new();
    let mut sys = DirMapping::new();
    add_root_entries(&mut root, fs.clone());
    add_sys_entries(&mut sys, fs.clone());
    root.add("sys", SimpleDir::new_maker(fs.clone(), Arc::new(sys)));
    SimpleDir::new_maker(fs, Arc::new(root))
}

#[def_test(serial)]
fn pty_nodes_report_limits_and_reject_root_writes() {
    let super_block = SimpleFs::new_with(&TEST_FILE_SYSTEM_TYPE, 0, build_basic_root).unwrap();
    let root = Mount::new_root(&super_block).root_path();
    for (path, expected) in [
        ("/sys/kernel/pty/max", ktty::tty::pty_limit()),
        ("/sys/kernel/pty/nr", ktty::tty::pty_allocated_count()),
    ] {
        // Root bypasses mode checks, so writes exercise the read-only backend.
        let file = Filename::new(path)
            .open_with_flags_at(
                &root,
                &root,
                OpenFlags::READ_WRITE.bits(),
                NodePermission::empty(),
                NodePermission::empty(),
                kcred::initial_cred(),
            )
            .unwrap();
        assert!(file.cred().has_sys_admin());
        assert_eq!(
            file.path().getattr().unwrap().mode.permission().bits(),
            0o444
        );
        let expected = format!("{expected}\n");
        let mut buf = [0u8; 32];
        let mut pos = 0;
        let count = file.read_from(&mut buf, &mut pos).unwrap();
        assert_eq!(&buf[..count], expected.as_bytes());
        assert_eq!(file.write(b"0\n"), Err(VfsError::BadFileDescriptor));
    }
}

/// `serial`: `printk` and `log_filter` own the process-global console filter, so
/// they must not interleave with another test that reads or writes it.
#[def_test(serial)]
fn printk_and_log_filter_nodes_read_and_write_the_console_filter() {
    let super_block = SimpleFs::new_with(&TEST_FILE_SYSTEM_TYPE, 0, build_basic_root).unwrap();
    let root = Mount::new_root(&super_block).root_path();
    let open = |path: &str| {
        Filename::new(path)
            .open_with_flags_at(
                &root,
                &root,
                OpenFlags::READ_WRITE.bits(),
                NodePermission::empty(),
                NodePermission::empty(),
                kcred::initial_cred(),
            )
            .unwrap()
    };
    let printk = open("/sys/kernel/printk");
    let log_filter = open("/sys/kernel/log_filter");

    // Restore whatever the boot (or a previous test) installed.
    let mut previous_buf = [0u8; klogger::MAX_FILTER_SPEC];
    let previous_len = klogger::filter_spec(&mut previous_buf).unwrap_or(0);

    // `printk` keeps its historical single-value contract: a level number (with
    // the trailing newline `echo` writes) or a level name, reflected by the
    // number the node reports.
    assert_eq!(printk.write(b"4\n"), Ok(2));
    assert_eq!(klogger::current_log_level(), log::LevelFilter::Debug);
    assert_eq!(printk.write(b"warn"), Ok(4));
    assert_eq!(klogger::current_log_level(), log::LevelFilter::Warn);
    let mut buf = [0u8; 64];
    let mut pos = 0;
    assert_eq!(printk.read_from(&mut buf, &mut pos), Ok(2));
    assert_eq!(&buf[..2], b"2\n");

    // A level write is a level write: the module filter is a separate knob, and
    // the level it sets becomes the spec's default.
    assert_eq!(log_filter.write(b"warn drv_uart=trace"), Ok(19));
    assert_eq!(printk.write(b"4"), Ok(1));
    let mut pos = 0;
    assert_eq!(log_filter.read_from(&mut buf, &mut pos), Ok(21));
    assert_eq!(&buf[..21], b"debug drv_uart=trace\n");
    // ...while `printk` still reports the bare level.
    let mut pos = 0;
    assert_eq!(printk.read_from(&mut buf, &mut pos), Ok(2));
    assert_eq!(&buf[..2], b"4\n");

    // Invalid input is rejected and leaves the filter unchanged.
    assert!(printk.write(b"banana").is_err());
    assert!(printk.write(b"6").is_err());
    assert!(log_filter.write(b"warn bogus=nonsense").is_err());
    let mut pos = 0;
    assert_eq!(log_filter.read_from(&mut buf, &mut pos), Ok(21));
    assert_eq!(&buf[..21], b"debug drv_uart=trace\n");

    // A spec that is accepted must also be reportable: the canonical text is
    // longer than any fixed 256-byte buffer once the rules are long, and a node
    // that accepts a write but fails the read-back with EINVAL is worse than
    // useless (the boot banner would silently show only the default level).
    let mut long_spec = String::from("warn");
    for index in 0..klogger::MAX_FILTER_RULES {
        // Distinct 24-byte targets, one per rule: the maximum the spec grammar
        // accepts.
        long_spec.push_str(&format!(" {index:0>24}=debug"));
    }
    // The canonical form is larger than the 256-byte buffer an earlier revision
    // used, which is exactly what made this read-back fail.
    assert!(long_spec.len() > 256);
    assert_eq!(log_filter.write(long_spec.as_bytes()), Ok(long_spec.len()));

    let mut long_buf = [0u8; klogger::MAX_FILTER_SPEC];
    let mut pos = 0;
    assert_eq!(
        log_filter.read_from(&mut long_buf, &mut pos),
        Ok(long_spec.len() + 1)
    );
    assert_eq!(
        core::str::from_utf8(&long_buf[..long_spec.len()]),
        Ok(long_spec.as_str())
    );

    if let Ok(text) = core::str::from_utf8(&previous_buf[..previous_len]) {
        klogger::set_log_level(text);
    } else {
        klogger::set_log_level_filter(log::LevelFilter::Warn);
    }
}

/// Linux's `proc_sys_permission` authorizes both opening and writing a sysctl
/// node with the caller's effective uid. Generic DAC would instead use fsuid,
/// so a root task that changed only fsuid must still open the node for write,
/// while an unprivileged task that changed fsuid to zero must not.
#[def_test(serial)]
fn inotify_sysctl_open_uses_effective_uid() {
    let super_block = SimpleFs::new_with(&TEST_FILE_SYSTEM_TYPE, 0, build_basic_root).unwrap();
    let root = Mount::new_root(&super_block).root_path();
    let path = "/sys/fs/inotify/max_user_watches";
    let open = |flags: OpenFlags, cred: kcred::Cred| {
        Filename::new(path).open_with_flags_at(
            &root,
            &root,
            flags.bits(),
            NodePermission::empty(),
            NodePermission::empty(),
            Arc::new(cred),
        )
    };

    let mut privileged = kcred::Cred::root();
    privileged.set_fsuid(1000);
    assert_eq!((privileged.euid(), privileged.fsuid()), (0, 1000));
    let file = open(OpenFlags::WRITE_ONLY, privileged).unwrap();
    assert_eq!(
        file.path().getattr().unwrap().mode.permission().bits(),
        0o644
    );

    // Linux's `proc_sys_setattr` rejects chmod/chown with EPERM. If they were
    // accepted, `getattr` would report a mode that the fixed permission hook
    // above does not enforce.
    let root_cred = kcred::initial_cred();
    assert_eq!(
        file.path()
            .chmod(NodePermission::from_bits_truncate(0o600), &root_cred),
        Err(VfsError::OperationNotPermitted)
    );
    assert_eq!(
        file.path().chown(Some(1000), Some(1000), &root_cred),
        Err(VfsError::OperationNotPermitted)
    );
    assert_eq!(
        file.path().getattr().unwrap().mode.permission().bits(),
        0o644
    );

    let mut dac_root = kcred::Cred::root();
    dac_root.set_reuid(None, Some(1000)).unwrap();
    dac_root.set_fsuid(0);
    assert_eq!((dac_root.euid(), dac_root.fsuid()), (1000, 0));
    assert_eq!(
        open(OpenFlags::WRITE_ONLY, dac_root).err(),
        Some(VfsError::PermissionDenied)
    );

    // Read access is not gated on euid: mode 0644 grants it to everyone.
    let mut dac_root = kcred::Cred::root();
    dac_root.set_reuid(None, Some(1000)).unwrap();
    dac_root.set_fsuid(0);
    assert!(open(OpenFlags::empty(), dac_root).is_ok());
}

/// Linux installs `proc_reg_file_ops` (whose `proc_reg_poll` falls back to
/// `DEFAULT_POLLMASK`) on the regular-file shape of a `/proc` entry, so those
/// entries answer `file_can_poll` and `epoll_ctl` accepts them. `/cmdline` is
/// the generated form and `/meminfo` the iterator form; both are regular files
/// and must be pollable.
///
/// The shape is what matters: a proc symlink inode is left with `i_fop = NULL`
/// (`fs/proc/inode.c`) and an `O_PATH` open installs `EmptyFileOperations`, so
/// neither answers `file_can_poll` — this test must not be read as licence to
/// give a poll handler to `/proc/<pid>/ns/*`, `exe` or `fd/N`.
#[def_test(serial)]
fn proc_nodes_are_pollable_like_linux() {
    let super_block = SimpleFs::new_with(&TEST_FILE_SYSTEM_TYPE, 0, |fs| {
        let mut root = DirMapping::new();
        add_root_entries(&mut root, fs.clone());
        crate::mem_nodes::root::add_root_entries(&mut root, fs.clone());
        SimpleDir::new_maker(fs, Arc::new(root))
    })
    .unwrap();
    let root = Mount::new_root(&super_block).root_path();

    for path in ["/cmdline", "/meminfo"] {
        let file = Filename::new(path)
            .open_with_flags_at(
                &root,
                &root,
                OpenFlags::empty().bits(),
                NodePermission::empty(),
                NodePermission::empty(),
                kcred::initial_cred(),
            )
            .unwrap();
        assert!(file.can_poll(), "{path} must be pollable");
    }
}

#[def_test]
fn quota_statistics_share_the_existing_filesystem_sysctl_tree() {
    let super_block = SimpleFs::new_with(&TEST_FILE_SYSTEM_TYPE, 0, build_basic_root).unwrap();
    let root = Mount::new_root(&super_block).root_path();
    for path in [
        "/sys/fs/quota/allocated_dquots",
        "/sys/fs/inotify/max_user_watches",
    ] {
        let file = Filename::new(path)
            .open_with_flags_at(
                &root,
                &root,
                OpenFlags::empty().bits(),
                NodePermission::empty(),
                NodePermission::empty(),
                kcred::initial_cred(),
            )
            .unwrap();
        let mut bytes = [0; 32];
        let length = file.read(&mut bytes).unwrap();
        let value = core::str::from_utf8(&bytes[..length]).unwrap();
        assert!(value.trim().parse::<usize>().is_ok());
    }
}

#[def_test(serial)]
fn cpuinfo_supports_partial_reads_and_rejects_writes() {
    let super_block = SimpleFs::new_with(&TEST_FILE_SYSTEM_TYPE, 0, build_basic_root).unwrap();
    let root = Mount::new_root(&super_block).root_path();
    let file = Filename::new("/cpuinfo")
        .open_with_flags_at(
            &root,
            &root,
            OpenFlags::READ_WRITE.bits(),
            NodePermission::empty(),
            NodePermission::empty(),
            kcred::initial_cred(),
        )
        .unwrap();
    let mut result = alloc::vec::Vec::new();
    let mut buffer = [0u8; 17];
    let mut position = 0;
    loop {
        let count = file.read_from(&mut buffer, &mut position).unwrap();
        if count == 0 {
            break;
        }
        result.extend_from_slice(&buffer[..count]);
    }
    let text = core::str::from_utf8(&result).unwrap();
    assert!(text.starts_with("processor\t:"));
    assert!(text.ends_with('\n'));
    assert_eq!(
        text.lines()
            .filter(|line| line.starts_with("processor\t:"))
            .count(),
        kcpu_id_map::nr_cpus()
    );
    assert_eq!(
        file.write(b"flags: avx\n"),
        Err(VfsError::BadFileDescriptor)
    );
}

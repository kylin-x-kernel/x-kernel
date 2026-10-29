// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{format, string::String, sync::Arc};

use kvfs::{
    DirMapping, FileSystemType, Filename, Mount, NodePermission, OpenFlags, SimpleDir, SimpleFs,
    VfsError,
};
use unittest::{assert, assert_eq, def_test};

use super::add_root_entries;

static TEST_FILE_SYSTEM_TYPE: FileSystemType = FileSystemType::internal("procfs-printk-test");

#[def_test(serial)]
fn pty_nodes_report_limits_and_reject_root_writes() {
    let super_block = SimpleFs::new_with(&TEST_FILE_SYSTEM_TYPE, 0, |fs| {
        let mut root = DirMapping::new();
        add_root_entries(&mut root, fs.clone());
        SimpleDir::new_maker(fs, Arc::new(root))
    });
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
        assert!(file.cred().is_privileged());
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
    let super_block = SimpleFs::new_with(&TEST_FILE_SYSTEM_TYPE, 0, |fs| {
        let mut root = DirMapping::new();
        add_root_entries(&mut root, fs.clone());
        SimpleDir::new_maker(fs, Arc::new(root))
    });
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
    });
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

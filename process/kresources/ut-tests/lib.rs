// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use krlimit::Rlimit;
use unittest::def_test;

use super::{FdTable, ProcessResources};

#[def_test]
fn test_process_resources_default_limits() {
    let resources = ProcessResources::new(0x80000);
    assert_eq!(resources.max_nofile(), 1024);
    assert_eq!(
        resources.rlimits.read()[linux_raw_sys::general::RLIMIT_STACK].current,
        0x80000
    );
    assert_eq!(resources.fd_table().unwrap().read().count(), 0);
}

#[def_test]
fn test_rlimit_accessors_update_selected_limit() {
    let resources = ProcessResources::new(0x80000);

    assert_eq!(
        resources
            .rlimit(linux_raw_sys::general::RLIMIT_NOFILE)
            .unwrap(),
        Rlimit::new(1024, 1024)
    );
    assert_eq!(
        resources
            .rlimit_values(linux_raw_sys::general::RLIMIT_NOFILE)
            .unwrap(),
        (1024, 1024)
    );

    resources
        .set_rlimit(
            linux_raw_sys::general::RLIMIT_NOFILE,
            Rlimit::new(512, 1024),
        )
        .unwrap();

    assert_eq!(
        resources
            .rlimit(linux_raw_sys::general::RLIMIT_NOFILE)
            .unwrap(),
        Rlimit::new(512, 1024)
    );
    assert_eq!(
        resources
            .rlimit_values(linux_raw_sys::general::RLIMIT_NOFILE)
            .unwrap(),
        (512, 1024)
    );
}

#[def_test]
fn test_set_rlimit_rejects_invalid_or_privileged_updates() {
    let resources = ProcessResources::new(0x80000);

    assert_eq!(
        resources
            .set_rlimit(
                linux_raw_sys::general::RLIMIT_NOFILE,
                Rlimit::new(2048, 1024)
            )
            .unwrap_err(),
        kerrno::KError::InvalidInput
    );

    assert_eq!(
        resources
            .set_rlimit(
                linux_raw_sys::general::RLIMIT_NOFILE,
                Rlimit::new(1024, 2048)
            )
            .unwrap_err(),
        kerrno::KError::OperationNotPermitted
    );
}

#[def_test]
fn test_exit_files_detaches_fd_table() {
    let resources = ProcessResources::new(0x80000);

    resources.exit_files();

    assert_eq!(
        resources.fd_table().err(),
        Some(kerrno::KError::NoSuchProcess)
    );
    assert_eq!(
        resources.replace_fd_table(FdTable::new_shared()).err(),
        Some(kerrno::KError::NoSuchProcess)
    );
    resources.exit_files();
}

#[def_test]
fn test_duplicate_file_from_rejects_detached_owner() {
    let resources = ProcessResources::new(0x80000);
    resources.exit_files();
    assert_eq!(
        resources.duplicate_file_from(0, 0, false),
        Err(kerrno::KError::NoSuchProcess)
    );
}

#[def_test]
fn test_shared_fd_table_lives_until_last_files_owner_exits() {
    let first = ProcessResources::new(0x80000);
    let second = ProcessResources::new(0x80000);
    let shared = first.fd_table().unwrap();
    let weak = Arc::downgrade(&shared);
    drop(second.replace_fd_table(shared).unwrap());

    first.exit_files();
    assert!(weak.upgrade().is_some());

    second.exit_files();
    assert!(weak.upgrade().is_none());
}

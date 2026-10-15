// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use krlimit::Rlimit;
use unittest::def_test;

use super::{FdTableOwner, ProcessResources};

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
        resources.replace_fd_table(FdTableOwner::new_shared()).err(),
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
    let shared = first.fork_fd_table(true).unwrap();
    let weak = Arc::downgrade(&shared);
    drop(second.replace_fd_table(shared).unwrap());

    first.exit_files();
    assert!(weak.upgrade().is_some());

    second.exit_files();
    assert!(weak.upgrade().is_none());
}

#[def_test]
fn test_rlimit_exchange_returns_replaced_pair_and_queries_without_mutation() {
    let resources = ProcessResources::new(0x80000);
    let resource = linux_raw_sys::general::RLIMIT_NOFILE;
    let original = resources.rlimit(resource);
    unittest::assert!(original.is_ok());
    let original = original.unwrap();
    let replacement = Rlimit::new(512, original.max);

    unittest::assert_eq!(
        resources.update_rlimit_and_get_old(resource, Some(replacement)),
        Ok(original)
    );
    unittest::assert_eq!(
        resources.update_rlimit_and_get_old(resource, None),
        Ok(replacement)
    );
    unittest::assert_eq!(resources.rlimit(resource), Ok(replacement));
    unittest::assert_eq!(
        resources.update_rlimit_and_get_old(resource, Some(replacement)),
        Ok(replacement)
    );

    let lowered = Rlimit::new(128, 256);
    unittest::assert_eq!(
        resources.update_rlimit_and_get_old(resource, Some(lowered)),
        Ok(replacement)
    );
    unittest::assert_eq!(
        resources.update_rlimit_and_get_old(resource, Some(replacement)),
        Err(kerrno::KError::OperationNotPermitted)
    );
    unittest::assert_eq!(resources.rlimit(resource), Ok(lowered));
    resources.exit_files();
    unittest::assert_eq!(
        resources.update_rlimit_and_get_old(resource, None),
        Ok(lowered)
    );
}

#[def_test]
fn test_rlimit_exchange_rejects_invalid_updates_without_mutation() {
    let resources = ProcessResources::new(0x80000);
    let resource = linux_raw_sys::general::RLIMIT_NOFILE;
    let original = resources.rlimit(resource);
    unittest::assert!(original.is_ok());
    let original = original.unwrap();

    for invalid_resource in [linux_raw_sys::general::RLIM_NLIMITS, u32::MAX] {
        unittest::assert_eq!(
            resources.update_rlimit_and_get_old(invalid_resource, None),
            Err(kerrno::KError::InvalidInput)
        );
        unittest::assert_eq!(
            resources.update_rlimit_and_get_old(invalid_resource, Some(original)),
            Err(kerrno::KError::InvalidInput)
        );
    }
    unittest::assert_eq!(
        resources.update_rlimit_and_get_old(resource, Some(Rlimit::new(2048, 1024))),
        Err(kerrno::KError::InvalidInput)
    );
    unittest::assert_eq!(resources.rlimit(resource), Ok(original));
    unittest::assert_eq!(
        resources.update_rlimit_and_get_old(resource, Some(Rlimit::new(1024, 2048))),
        Err(kerrno::KError::OperationNotPermitted)
    );
    unittest::assert_eq!(resources.rlimit(resource), Ok(original));
}

#[def_test]
fn temporary_table_references_do_not_force_unshare() {
    let resources = ProcessResources::new(0x80000);
    let table = resources.fd_table().unwrap();
    resources.unshare_fd_table().unwrap();
    assert_eq!(
        table.read().lock_owner(),
        resources.fd_table().unwrap().read().lock_owner()
    );
}

#[def_test]
fn shared_bindings_still_copy_on_unshare() {
    let first = ProcessResources::new(0x80000);
    let second = ProcessResources::new(0x80000);
    drop(
        second
            .replace_fd_table(first.fork_fd_table(true).unwrap())
            .unwrap(),
    );
    let old_table = first.fd_table().unwrap();
    second.unshare_fd_table().unwrap();
    assert_eq!(
        old_table.read().lock_owner(),
        first.fd_table().unwrap().read().lock_owner()
    );
    assert_ne!(
        old_table.read().lock_owner(),
        second.fd_table().unwrap().read().lock_owner()
    );
}

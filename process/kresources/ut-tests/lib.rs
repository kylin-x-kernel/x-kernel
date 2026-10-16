// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kfd::FdTableOwner;
use krlimit::Rlimit;
use unittest::def_test;

use super::{ProcessResources, TaskFiles};

#[def_test]
fn test_process_resources_default_limits() {
    let resources = ProcessResources::new(0x80000);
    assert_eq!(resources.max_nofile(), 1024);
    assert_eq!(
        resources.rlimits.read()[linux_raw_sys::general::RLIMIT_STACK].current,
        0x80000
    );
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
    let files = TaskFiles::new(FdTableOwner::new_shared());

    files.exit_files();

    assert_eq!(files.fd_table().err(), Some(kerrno::KError::NoSuchProcess));
    assert_eq!(files.fork(true).err(), Some(kerrno::KError::NoSuchProcess));
}

#[def_test]
fn test_duplicate_file_from_rejects_detached_owner() {
    let files = TaskFiles::new(FdTableOwner::new_shared());
    files.exit_files();
    assert_eq!(
        files.duplicate_file_from(1024, 0, 0, false),
        Err(kerrno::KError::NoSuchProcess)
    );
}

#[def_test]
fn test_shared_fd_table_lives_until_last_files_owner_exits() {
    let first = TaskFiles::new(FdTableOwner::new_shared());
    let second = first.fork(true).unwrap();
    // A temporary reference must not count as a binding.
    let reference = first.fd_table().unwrap();
    let lock_owner = reference.read().lock_owner();

    first.exit_files();
    // The surviving binding still owns the descriptors.
    assert_eq!(reference.read().lock_owner(), lock_owner);
    assert!(second.fd_table().is_ok());

    second.exit_files();
    // The last binding drained the table even though `reference` is retained.
    assert_eq!(reference.read().count(), 0);
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
    // The files owner is per-task now; detaching it must not affect the limits.
    TaskFiles::new(FdTableOwner::new_shared()).exit_files();
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
    let files = TaskFiles::new(FdTableOwner::new_shared());
    let table = files.fd_table().unwrap();
    files.unshare_fd_table().unwrap();
    assert_eq!(
        table.read().lock_owner(),
        files.fd_table().unwrap().read().lock_owner()
    );
}

#[def_test]
fn shared_bindings_still_copy_on_unshare() {
    let first = TaskFiles::new(FdTableOwner::new_shared());
    let second = first.fork(true).unwrap();
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

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kvfs::{MountFlags, SuperBlockFlags};
use unittest::{assert, assert_eq, def_test};

use super::LEGACY_MOUNT_DATA_SIZE;

#[def_test]
fn mount_data_page_preserves_opaque_bytes_and_zero_fills_after_fault() {
    let input = [0xff, 0, 0x80, b','];
    let copied =
        super::copy_mount_data_page(|offset| input.get(offset).copied().ok_or(())).unwrap();

    assert_eq!(&copied[..input.len()], &input);
    assert!(copied[input.len()..].iter().all(|byte| *byte == 0));
}

#[def_test]
fn mount_data_page_reserves_last_byte_for_text_termination() {
    let copied = super::copy_mount_data_page::<()>(|_| Ok(0xff)).unwrap();

    assert_eq!(copied.len(), LEGACY_MOUNT_DATA_SIZE);
    assert_eq!(copied[0], 0xff);
    assert_eq!(copied[LEGACY_MOUNT_DATA_SIZE - 1], 0);
}

#[def_test]
fn mount_data_page_rejects_an_unreadable_first_byte() {
    assert!(super::copy_mount_data_page::<()>(|_| Err(())).is_err());
}

#[def_test]
fn test_superblock_flags_from_mount_only_options_are_filtered() {
    let flags = linux_raw_sys::general::MS_NODEV
        | linux_raw_sys::general::MS_NOEXEC
        | linux_raw_sys::general::MS_NOSUID
        | linux_raw_sys::general::MS_NOATIME
        | linux_raw_sys::general::MS_NODIRATIME
        | linux_raw_sys::general::MS_NOSYMFOLLOW;

    assert_eq!(super::superblock_flags(flags), SuperBlockFlags::empty());
}

#[def_test]
fn test_superblock_flags_preserve_readonly() {
    assert_eq!(
        super::superblock_flags(linux_raw_sys::general::MS_RDONLY),
        SuperBlockFlags::RDONLY
    );
}

#[def_test]
fn test_per_mount_flags_preserve_mount_options() {
    let flags = linux_raw_sys::general::MS_RDONLY
        | linux_raw_sys::general::MS_NODEV
        | linux_raw_sys::general::MS_NOEXEC;
    let result = super::per_mount_flags(flags, MountFlags::empty());

    assert!(result.contains(MountFlags::RDONLY));
    assert!(result.contains(MountFlags::NODEV));
    assert!(result.contains(MountFlags::NOEXEC));
}

#[def_test]
fn private_propagation_accepts_optional_recursive_scope() {
    assert!(super::validate_propagation_flags(
            linux_raw_sys::general::MS_PRIVATE,
            linux_raw_sys::general::MS_PRIVATE
        )
        .is_ok());
    assert!(
        super::validate_propagation_flags(
            linux_raw_sys::general::MS_PRIVATE | linux_raw_sys::general::MS_REC,
            linux_raw_sys::general::MS_PRIVATE
        )
        .is_ok()
    );
}

#[def_test]
fn private_propagation_accepts_silent_flag() {
    assert!(
        super::validate_propagation_flags(
            linux_raw_sys::general::MS_PRIVATE | linux_raw_sys::general::MS_SILENT,
            linux_raw_sys::general::MS_PRIVATE
        )
        .is_ok()
    );
}

#[def_test]
fn private_propagation_rejects_unrelated_flags() {
    assert!(
        super::validate_propagation_flags(
            linux_raw_sys::general::MS_PRIVATE | linux_raw_sys::general::MS_RDONLY,
            linux_raw_sys::general::MS_PRIVATE
        )
        .is_err()
    );
}

#[def_test]
fn mount_privilege_rejects_a_non_root_effective_uid() {
    assert_eq!(
        super::require_mount_privilege(&kcred::Cred::new(1000, 1000)),
        Err(kerrno::KError::OperationNotPermitted)
    );
}

#[def_test]
fn mount_privilege_accepts_root_credentials() {
    assert!(super::require_mount_privilege(&kcred::Cred::root()).is_ok());
}

#[def_test]
fn mount_privilege_uses_the_effective_uid_not_the_real_uid() {
    // A setuid-root program that drops its effective UID keeps real UID 0,
    // and must lose mount privilege with it. `Cred::new` cannot express
    // that state because it sets every user ID at once.
    let mut cred = kcred::Cred::root();
    cred.set_resuid(Some(0), Some(1000), Some(0)).unwrap();

    assert_eq!(cred.ruid(), 0);
    assert_eq!(cred.euid(), 1000);
    assert_eq!(
        super::require_mount_privilege(&cred),
        Err(kerrno::KError::OperationNotPermitted)
    );
}

#[def_test]
fn slave_propagation_accepts_recursive_scope_only() {
    assert!(
        super::validate_propagation_flags(
            linux_raw_sys::general::MS_SLAVE | linux_raw_sys::general::MS_REC,
            linux_raw_sys::general::MS_SLAVE
        )
        .is_ok()
    );
    // runc's `prepareRoot()` sends exactly `MS_SLAVE | MS_REC`.
    assert!(
        super::validate_propagation_flags(
            linux_raw_sys::general::MS_SLAVE
                | linux_raw_sys::general::MS_REC
                | linux_raw_sys::general::MS_SILENT,
            linux_raw_sys::general::MS_SLAVE
        )
        .is_ok()
    );
    assert!(
        super::validate_propagation_flags(
            linux_raw_sys::general::MS_SLAVE | linux_raw_sys::general::MS_PRIVATE,
            linux_raw_sys::general::MS_SLAVE
        )
        .is_err()
    );
}

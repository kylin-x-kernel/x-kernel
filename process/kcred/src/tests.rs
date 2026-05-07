// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use crate::Credentials;

#[def_test]
fn test_credentials_root_initial_state() {
    let credentials = Credentials::root();

    assert_eq!(credentials.ruid(), 0);
    assert_eq!(credentials.euid(), 0);
    assert_eq!(credentials.suid(), 0);
    assert_eq!(credentials.fsuid(), 0);
    assert_eq!(credentials.rgid(), 0);
    assert_eq!(credentials.egid(), 0);
    assert_eq!(credentials.sgid(), 0);
    assert_eq!(credentials.fsgid(), 0);
    assert_eq!(credentials.supplementary_groups(), alloc::vec![]);
}

#[def_test]
fn test_credentials_clone_is_deep_copy() {
    let mut parent = Credentials::new(1000, 100);
    let child = parent.clone();

    parent.set_resuid_unchecked(None, Some(2000), Some(3000));
    parent.set_resgid_unchecked(None, Some(200), Some(300));

    assert_eq!(child.ruid(), 1000);
    assert_eq!(child.euid(), 1000);
    assert_eq!(child.suid(), 1000);
    assert_eq!(child.fsuid(), 1000);
    assert_eq!(child.rgid(), 100);
    assert_eq!(child.egid(), 100);
    assert_eq!(child.sgid(), 100);
    assert_eq!(child.fsgid(), 100);
}

#[def_test]
fn test_credentials_effective_ids_update_filesystem_ids() {
    let mut credentials = Credentials::new(1000, 100);

    credentials.set_resuid_unchecked(None, Some(2000), None);
    credentials.set_resgid_unchecked(None, Some(200), None);

    assert_eq!(credentials.euid(), 2000);
    assert_eq!(credentials.fsuid(), 2000);
    assert_eq!(credentials.egid(), 200);
    assert_eq!(credentials.fsgid(), 200);
}

#[def_test]
fn test_credentials_exec_resets_saved_ids() {
    let mut credentials = Credentials::new(1000, 100);
    credentials.set_resuid_unchecked(None, Some(2000), Some(3000));
    credentials.set_resgid_unchecked(None, Some(200), Some(300));

    credentials.apply_exec();

    assert_eq!(credentials.ruid(), 1000);
    assert_eq!(credentials.euid(), 2000);
    assert_eq!(credentials.suid(), 2000);
    assert_eq!(credentials.rgid(), 100);
    assert_eq!(credentials.egid(), 200);
    assert_eq!(credentials.sgid(), 200);
}

#[def_test]
fn test_unprivileged_setuid_can_switch_to_saved_uid_only() {
    let mut credentials = Credentials::new(1000, 100);
    credentials.set_resuid_unchecked(None, Some(2000), Some(3000));

    assert!(credentials.set_uid(3000).is_ok());
    assert_eq!(credentials.ruid(), 1000);
    assert_eq!(credentials.euid(), 3000);
    assert_eq!(credentials.suid(), 3000);
    assert_eq!(credentials.fsuid(), 3000);
    assert!(credentials.set_uid(2000).is_err());
}

#[def_test]
fn test_unprivileged_setuid_accepts_current_effective_uid() {
    let mut credentials = Credentials::new(1000, 100);
    credentials.set_resuid_unchecked(None, Some(2000), Some(3000));

    assert!(credentials.set_uid(2000).is_ok());

    assert_eq!(credentials.ruid(), 1000);
    assert_eq!(credentials.euid(), 2000);
    assert_eq!(credentials.suid(), 3000);
    assert_eq!(credentials.fsuid(), 2000);
}

#[def_test]
fn test_unprivileged_setgid_accepts_current_effective_gid() {
    let mut credentials = Credentials::new(1000, 100);
    credentials.set_resgid_unchecked(None, Some(200), Some(300));

    assert!(credentials.set_gid(200).is_ok());

    assert_eq!(credentials.rgid(), 100);
    assert_eq!(credentials.egid(), 200);
    assert_eq!(credentials.sgid(), 300);
    assert_eq!(credentials.fsgid(), 200);
}

#[def_test]
fn test_privileged_setuid_sets_all_user_ids() {
    let mut credentials = Credentials::root();

    assert!(credentials.set_uid(1000).is_ok());
    assert_eq!(credentials.ruid(), 1000);
    assert_eq!(credentials.euid(), 1000);
    assert_eq!(credentials.suid(), 1000);
    assert_eq!(credentials.fsuid(), 1000);
}

#[def_test]
fn test_setresuid_rejects_unassociated_uid_without_privilege() {
    let mut credentials = Credentials::new(1000, 100);

    assert!(credentials.set_resuid(None, Some(2000), None).is_err());
    assert_eq!(credentials.euid(), 1000);
}

#[def_test]
fn test_setreuid_saved_uid_tracks_final_effective_uid() {
    let mut credentials = Credentials::root();
    credentials.set_resuid_unchecked(Some(1000), Some(1000), Some(2000));

    assert!(credentials.set_reuid(None, Some(2000)).is_ok());

    assert_eq!(credentials.ruid(), 1000);
    assert_eq!(credentials.euid(), 2000);
    assert_eq!(credentials.suid(), 2000);
}

#[def_test]
fn test_setresuid_resets_fsuid_to_effective_uid() {
    let mut credentials = Credentials::root();
    assert_eq!(credentials.set_fsuid(1234), 0);
    assert_eq!(credentials.fsuid(), 1234);

    assert!(credentials.set_resuid(None, None, Some(1000)).is_ok());

    assert_eq!(credentials.euid(), 0);
    assert_eq!(credentials.fsuid(), 0);
}

#[def_test]
fn test_setresuid_noop_preserves_explicit_fsuid() {
    let mut credentials = Credentials::root();
    assert_eq!(credentials.set_fsuid(1234), 0);
    assert_eq!(credentials.fsuid(), 1234);

    assert!(credentials.set_resuid(Some(0), None, None).is_ok());

    assert_eq!(credentials.ruid(), 0);
    assert_eq!(credentials.euid(), 0);
    assert_eq!(credentials.fsuid(), 1234);
}

#[def_test]
fn test_setresuid_current_euid_resets_explicit_fsuid() {
    let mut credentials = Credentials::root();
    assert_eq!(credentials.set_fsuid(1234), 0);
    assert_eq!(credentials.fsuid(), 1234);

    assert!(credentials.set_resuid(None, Some(0), None).is_ok());

    assert_eq!(credentials.euid(), 0);
    assert_eq!(credentials.fsuid(), 0);
}

#[def_test]
fn test_setfsuid_returns_old_id_and_ignores_unpermitted_change() {
    let mut credentials = Credentials::new(1000, 100);

    assert_eq!(credentials.set_fsuid(2000), 1000);
    assert_eq!(credentials.fsuid(), 1000);
    assert_eq!(credentials.set_fsuid(1000), 1000);
    assert_eq!(credentials.fsuid(), 1000);
}

#[def_test]
fn test_supplementary_groups_are_sorted_and_preserve_duplicates() {
    let mut credentials = Credentials::new(1000, 100);

    credentials.set_supplementary_groups(alloc::vec![3, 1, 3, 2]);

    assert_eq!(credentials.supplementary_groups(), alloc::vec![1, 2, 3, 3]);
}

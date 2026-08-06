// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use unittest::def_test;

use crate::{Cred, initial_cred};

#[def_test]
fn test_credentials_root_initial_state() {
    let credentials = Cred::root();

    assert_eq!(credentials.ruid(), 0);
    assert_eq!(credentials.euid(), 0);
    assert_eq!(credentials.suid(), 0);
    assert_eq!(credentials.fsuid(), 0);
    assert_eq!(credentials.rgid(), 0);
    assert_eq!(credentials.egid(), 0);
    assert_eq!(credentials.sgid(), 0);
    assert_eq!(credentials.fsgid(), 0);
    assert_eq!(credentials.supplementary_groups(), &[]);
}

#[def_test]
fn test_credentials_clone_is_deep_copy() {
    let mut parent = Cred::new(1000, 100);
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
    let mut credentials = Cred::new(1000, 100);

    credentials.set_resuid_unchecked(None, Some(2000), None);
    credentials.set_resgid_unchecked(None, Some(200), None);

    assert_eq!(credentials.euid(), 2000);
    assert_eq!(credentials.fsuid(), 2000);
    assert_eq!(credentials.egid(), 200);
    assert_eq!(credentials.fsgid(), 200);
}

#[def_test]
fn test_credentials_exec_resets_saved_ids() {
    let mut credentials = Cred::new(1000, 100);
    credentials.set_resuid_unchecked(None, Some(2000), Some(3000));
    credentials.set_resgid_unchecked(None, Some(200), Some(300));
    credentials.keep_caps_enable().unwrap();

    credentials.apply_exec();

    assert_eq!(credentials.ruid(), 1000);
    assert_eq!(credentials.euid(), 2000);
    assert_eq!(credentials.suid(), 2000);
    assert_eq!(credentials.rgid(), 100);
    assert_eq!(credentials.egid(), 200);
    assert_eq!(credentials.sgid(), 200);
    assert!(!credentials.keep_caps());
}

#[def_test]
fn test_keep_caps_set_get_and_lock() {
    let mut credentials = Cred::root();
    assert!(!credentials.keep_caps());

    credentials.keep_caps_enable().unwrap();
    assert!(credentials.keep_caps());

    credentials.lock_keep_caps_for_test();
    assert!(credentials.keep_caps_disable().is_err());
    assert!(credentials.keep_caps());
}

#[def_test]
fn test_unprivileged_setuid_can_switch_to_saved_uid_only() {
    let mut credentials = Cred::new(1000, 100);
    credentials.set_resuid_unchecked(None, Some(2000), Some(3000));

    assert!(credentials.set_uid(3000).is_ok());
    assert_eq!(credentials.ruid(), 1000);
    assert_eq!(credentials.euid(), 3000);
    assert_eq!(credentials.suid(), 3000);
    assert_eq!(credentials.fsuid(), 3000);
    assert!(credentials.set_uid(2000).is_err());
}

#[def_test]
fn test_unprivileged_setuid_rejects_current_effective_uid() {
    let mut credentials = Cred::new(1000, 100);
    credentials.set_resuid_unchecked(None, Some(2000), Some(3000));

    assert!(credentials.set_uid(2000).is_err());

    assert_eq!(credentials.ruid(), 1000);
    assert_eq!(credentials.euid(), 2000);
    assert_eq!(credentials.suid(), 3000);
    assert_eq!(credentials.fsuid(), 2000);
}

#[def_test]
fn test_unprivileged_setgid_rejects_current_effective_gid() {
    let mut credentials = Cred::new(1000, 100);
    credentials.set_resgid_unchecked(None, Some(200), Some(300));

    assert!(credentials.set_gid(200).is_err());

    assert_eq!(credentials.rgid(), 100);
    assert_eq!(credentials.egid(), 200);
    assert_eq!(credentials.sgid(), 300);
    assert_eq!(credentials.fsgid(), 200);
}

#[def_test]
fn test_privileged_setuid_sets_all_user_ids() {
    let mut credentials = Cred::root();

    assert!(credentials.set_uid(1000).is_ok());
    assert_eq!(credentials.ruid(), 1000);
    assert_eq!(credentials.euid(), 1000);
    assert_eq!(credentials.suid(), 1000);
    assert_eq!(credentials.fsuid(), 1000);
}

#[def_test]
fn test_setresuid_rejects_unassociated_uid_without_privilege() {
    let mut credentials = Cred::new(1000, 100);

    assert!(credentials.set_resuid(None, Some(2000), None).is_err());
    assert_eq!(credentials.euid(), 1000);
}

#[def_test]
fn test_setreuid_saved_uid_tracks_final_effective_uid() {
    let mut credentials = Cred::root();
    credentials.set_resuid_unchecked(Some(1000), Some(1000), Some(2000));

    assert!(credentials.set_reuid(None, Some(2000)).is_ok());

    assert_eq!(credentials.ruid(), 1000);
    assert_eq!(credentials.euid(), 2000);
    assert_eq!(credentials.suid(), 2000);
}

#[def_test]
fn test_setreuid_noop_resets_fsuid_to_effective_uid() {
    let mut credentials = Cred::root();
    assert_eq!(credentials.set_fsuid(1234), 0);

    assert!(credentials.set_reuid(None, None).is_ok());

    assert_eq!(credentials.fsuid(), credentials.euid());
}

#[def_test]
fn test_setregid_noop_resets_fsgid_to_effective_gid() {
    let mut credentials = Cred::root();
    assert_eq!(credentials.set_fsgid(1234), 0);

    assert!(credentials.set_regid(None, None).is_ok());

    assert_eq!(credentials.fsgid(), credentials.egid());
}

#[def_test]
fn test_setresuid_resets_fsuid_to_effective_uid() {
    let mut credentials = Cred::root();
    assert_eq!(credentials.set_fsuid(1234), 0);
    assert_eq!(credentials.fsuid(), 1234);

    assert!(credentials.set_resuid(None, None, Some(1000)).is_ok());

    assert_eq!(credentials.euid(), 0);
    assert_eq!(credentials.fsuid(), 0);
}

#[def_test]
fn test_setresuid_noop_preserves_explicit_fsuid() {
    let mut credentials = Cred::root();
    assert_eq!(credentials.set_fsuid(1234), 0);
    assert_eq!(credentials.fsuid(), 1234);

    assert!(credentials.set_resuid(Some(0), None, None).is_ok());

    assert_eq!(credentials.ruid(), 0);
    assert_eq!(credentials.euid(), 0);
    assert_eq!(credentials.fsuid(), 1234);
}

#[def_test]
fn test_setresuid_current_euid_resets_explicit_fsuid() {
    let mut credentials = Cred::root();
    assert_eq!(credentials.set_fsuid(1234), 0);
    assert_eq!(credentials.fsuid(), 1234);

    assert!(credentials.set_resuid(None, Some(0), None).is_ok());

    assert_eq!(credentials.euid(), 0);
    assert_eq!(credentials.fsuid(), 0);
}

#[def_test]
fn test_setfsuid_returns_old_id_and_ignores_unpermitted_change() {
    let mut credentials = Cred::new(1000, 100);

    assert_eq!(credentials.set_fsuid(2000), 1000);
    assert_eq!(credentials.fsuid(), 1000);
    assert_eq!(credentials.set_fsuid(1000), 1000);
    assert_eq!(credentials.fsuid(), 1000);
}

#[def_test]
fn test_supplementary_groups_are_sorted_and_preserve_duplicates() {
    let mut credentials = Cred::new(1000, 100);

    credentials.set_supplementary_groups(alloc::vec![3, 1, 3, 2]);

    assert_eq!(credentials.supplementary_groups(), &[1, 2, 3, 3]);
}

#[def_test]
fn test_prepared_credential_does_not_mutate_committed_credential() {
    let committed = Arc::new(Cred::new(1000, 100));
    let mut prepared = committed.prepare();

    assert_eq!(prepared.set_fsuid(1000), 1000);
    prepared.set_supplementary_groups(alloc::vec![200]);

    assert_eq!(committed.fsuid(), 1000);
    assert_eq!(committed.supplementary_groups(), &[]);
}

#[def_test]
fn test_access_credential_uses_real_ids_without_changing_source() {
    let mut cred = Cred::new(1000, 100);
    cred.set_resuid_unchecked(None, Some(2000), Some(3000));
    cred.set_resgid_unchecked(None, Some(200), Some(300));

    let access = cred.for_access();

    assert_eq!(access.fsuid(), 1000);
    assert_eq!(access.fsgid(), 100);
    assert_eq!(cred.fsuid(), 2000);
    assert_eq!(cred.fsgid(), 200);
}

#[def_test]
fn test_initial_credential_is_shared() {
    assert!(Arc::ptr_eq(&initial_cred(), &initial_cred()));
}

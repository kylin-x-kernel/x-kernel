// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use kerrno::KError;
use unittest::def_test;

use crate::{
    CAP_CHOWN, CAP_DAC_OVERRIDE, CAP_LAST_CAP, CAP_SETPCAP, CAP_SETUID, CAP_SYS_ADMIN,
    CAP_SYS_NICE, CAP_SYS_PTRACE, CapSet, CapSets, Cred, INVALID_GID, INVALID_UID,
    PtraceAccessMode, initial_cred,
};

fn cap(capability: u32) -> CapSet {
    CapSet::from_bits(1 << capability)
}

fn credential_with_capabilities(uid: u32, gid: u32, effective: CapSet, permitted: CapSet) -> Cred {
    let mut credentials = Cred::root();
    credentials.set_resuid_unchecked(Some(uid), Some(uid), Some(uid));
    credentials.set_resgid_unchecked(Some(gid), Some(gid), Some(gid));
    let transitioned = credentials
        .set_capabilities(CapSets::new(effective, permitted, CapSet::EMPTY))
        .is_ok();
    assert!(
        transitioned,
        "test credentials reject the requested capability sets"
    );
    credentials
}

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
    assert_eq!(credentials.capabilities(), CapSets::FULL_PRIVILEGED);
    assert_eq!(credentials.cap_bounding(), CapSet::FULL);
}

#[def_test]
fn nonroot_initial_credential_has_no_process_capabilities() {
    let credentials = Cred::new(1000, 100);

    assert_eq!(credentials.capabilities(), CapSets::EMPTY);
    assert_eq!(credentials.cap_bounding(), CapSet::FULL);
}

#[def_test]
fn capability_authority_uses_the_effective_set_not_the_effective_uid() {
    let mut credentials = Cred::root();
    assert!(credentials.has_sys_admin());

    credentials
        .set_capabilities(CapSets::new(
            cap(CAP_SYS_ADMIN),
            CapSet::FULL,
            CapSet::EMPTY,
        ))
        .unwrap();
    credentials.set_resuid_unchecked(None, Some(1000), None);
    credentials.set_fsuid(1000);
    assert!(credentials.has_sys_admin());

    credentials
        .set_capabilities(CapSets::new(CapSet::EMPTY, CapSet::FULL, CapSet::EMPTY))
        .unwrap();
    assert!(!credentials.has_sys_admin());
    assert!(!Cred::new(1000, 0).has_sys_admin());
}

#[def_test]
fn root_euid_predicate_ignores_the_filesystem_uid() {
    let mut credentials = Cred::root();
    assert!(credentials.has_root_euid());

    credentials.set_reuid(None, Some(1000)).unwrap();
    assert!(!credentials.has_root_euid());

    credentials.set_fsuid(0);
    assert_eq!(credentials.fsuid(), 0);
    assert!(!credentials.has_root_euid());
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
fn tty_configuration_authority_uses_the_effective_set() {
    let mut cred = Cred::root();
    assert!(cred.has_sys_tty_config());

    cred.set_capabilities(CapSets::new(
        cap(crate::CAP_SYS_TTY_CONFIG),
        CapSet::FULL,
        CapSet::EMPTY,
    ))
    .unwrap();
    cred.set_resuid_unchecked(None, Some(1000), None);
    cred.set_fsuid(1000);
    assert!(cred.has_sys_tty_config());

    cred.set_capabilities(CapSets::new(CapSet::EMPTY, CapSet::FULL, CapSet::EMPTY))
        .unwrap();
    assert!(!cred.has_sys_tty_config());
    assert!(!Cred::new(1000, 0).has_sys_tty_config());
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

    credentials.apply_exec(false, false);

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
fn securebits_set_get_validate_locks_and_require_setpcap() {
    let mut credentials = Cred::root();
    assert_eq!(credentials.securebits(), 0);

    // NO_SETUID_FIXUP | NO_CAP_AMBIENT_RAISE.
    credentials.set_securebits(0x44).unwrap();
    assert_eq!(credentials.securebits(), 0x44);

    // Modern exec-check settings and their locks are supported.
    credentials.set_securebits(0x700).unwrap();
    assert_eq!(credentials.securebits(), 0x700);
    credentials.set_securebits(0xf00).unwrap();
    assert_eq!(credentials.securebits(), 0xf00);

    // Unsupported bits are rejected without narrowing a full-width value.
    assert!(credentials.set_securebits(1 << 12).is_err());
    assert!(credentials.set_securebits(1 << 40).is_err());
    assert_eq!(credentials.securebits(), 0xf00);

    // NO_SETUID_FIXUP_LOCKED protects NO_SETUID_FIXUP.
    let mut locked = Cred::root();
    locked.set_securebits(0x44).unwrap();
    locked.set_securebits(0x4c).unwrap();
    assert!(locked.set_securebits(0x48).is_err());
    assert!(locked.set_securebits(0x44).is_err());
    assert_eq!(locked.securebits(), 0x4c);
    locked.set_securebits(0x4c).unwrap();

    // A caller without CAP_SETPCAP may change only the exec-check bits and
    // their locks, and must make an actual change.
    let mut unprivileged = Cred::new(1000, 100);
    assert!(unprivileged.set_securebits(0).is_err());
    assert!(unprivileged.set_securebits(0x4).is_err());
    assert_eq!(unprivileged.securebits(), 0);
    unprivileged.set_securebits(0x700).unwrap();
    assert_eq!(unprivileged.securebits(), 0x700);
    assert!(unprivileged.set_securebits(0x700).is_err());
    assert!(unprivileged.set_securebits(0x500).is_err());
    assert!(unprivileged.set_securebits(0x701).is_err());
    assert_eq!(unprivileged.securebits(), 0x700);
}

#[def_test]
fn ambient_raise_requires_permitted_and_inheritable_and_honors_securebit() {
    let mut credentials = Cred::root();
    credentials
        .set_capabilities(CapSets::new(cap(CAP_CHOWN), cap(CAP_CHOWN), cap(CAP_CHOWN)))
        .unwrap();
    assert_eq!(credentials.cap_ambient(), CapSet::EMPTY);

    credentials.raise_cap_ambient(CAP_CHOWN).unwrap();
    assert_eq!(credentials.cap_ambient(), cap(CAP_CHOWN));
    assert!(credentials.has_ambient_cap(CAP_CHOWN));
    credentials.raise_cap_ambient(CAP_CHOWN).unwrap();
    assert_eq!(credentials.cap_ambient(), cap(CAP_CHOWN));

    // Lowering an absent capability is still a successful no-op.
    credentials.lower_cap_ambient(CAP_SYS_ADMIN).unwrap();
    credentials.lower_cap_ambient(CAP_CHOWN).unwrap();
    assert_eq!(credentials.cap_ambient(), CapSet::EMPTY);
    assert_eq!(credentials.securebits(), 0);

    // CAP_SYS_ADMIN is neither permitted nor inheritable here.
    assert_eq!(
        credentials.raise_cap_ambient(CAP_SYS_ADMIN),
        Err(KError::OperationNotPermitted)
    );
    assert_eq!(
        credentials.raise_cap_ambient(CAP_LAST_CAP + 1),
        Err(KError::InvalidInput)
    );

    let mut no_ambient_raise = Cred::root();
    no_ambient_raise.set_securebits(1 << 6).unwrap();
    no_ambient_raise
        .set_capabilities(CapSets::new(cap(CAP_CHOWN), cap(CAP_CHOWN), cap(CAP_CHOWN)))
        .unwrap();
    assert_eq!(
        no_ambient_raise.raise_cap_ambient(CAP_CHOWN),
        Err(KError::OperationNotPermitted)
    );
    assert_eq!(no_ambient_raise.cap_ambient(), CapSet::EMPTY);
}

#[def_test]
fn capset_masks_ambient_capabilities_with_the_new_sets() {
    let mut credentials = Cred::root();
    credentials
        .set_capabilities(CapSets::new(cap(CAP_CHOWN), cap(CAP_CHOWN), cap(CAP_CHOWN)))
        .unwrap();
    credentials.raise_cap_ambient(CAP_CHOWN).unwrap();

    credentials
        .set_capabilities(CapSets::new(CapSet::EMPTY, CapSet::EMPTY, cap(CAP_CHOWN)))
        .unwrap();
    assert_eq!(credentials.cap_ambient(), CapSet::EMPTY);

    let mut inherited_only = Cred::root();
    inherited_only
        .set_capabilities(CapSets::new(cap(CAP_CHOWN), cap(CAP_CHOWN), cap(CAP_CHOWN)))
        .unwrap();
    inherited_only.raise_cap_ambient(CAP_CHOWN).unwrap();
    inherited_only
        .set_capabilities(CapSets::new(cap(CAP_CHOWN), cap(CAP_CHOWN), CapSet::EMPTY))
        .unwrap();
    assert_eq!(inherited_only.cap_ambient(), CapSet::EMPTY);
}

#[def_test]
fn exec_preserves_ambient_capabilities_and_merges_them_into_permitted() {
    let mut credentials = Cred::root();
    credentials
        .set_capabilities(CapSets::new(cap(CAP_CHOWN), cap(CAP_CHOWN), cap(CAP_CHOWN)))
        .unwrap();
    credentials.raise_cap_ambient(CAP_CHOWN).unwrap();
    credentials.set_resuid_unchecked(Some(1000), Some(1000), Some(1000));

    credentials.apply_exec(false, false);

    assert_eq!(credentials.cap_ambient(), cap(CAP_CHOWN));
    assert_eq!(
        credentials.capabilities(),
        CapSets::new(cap(CAP_CHOWN), cap(CAP_CHOWN), cap(CAP_CHOWN))
    );
}

#[def_test]
fn setuid_away_from_root_clears_ambient_capabilities_even_with_keep_caps() {
    let mut credentials = Cred::root();
    credentials
        .set_capabilities(CapSets::new(
            cap(CAP_SETUID).union(cap(CAP_CHOWN)),
            cap(CAP_SETUID).union(cap(CAP_CHOWN)),
            cap(CAP_CHOWN),
        ))
        .unwrap();
    credentials.raise_cap_ambient(CAP_CHOWN).unwrap();
    credentials.keep_caps_enable().unwrap();

    credentials.set_uid(1000).unwrap();

    assert_eq!(credentials.cap_ambient(), CapSet::EMPTY);
    assert_eq!(
        credentials.capabilities().permitted(),
        cap(CAP_SETUID).union(cap(CAP_CHOWN))
    );
    assert_eq!(credentials.capabilities().effective(), CapSet::EMPTY);
    assert_eq!(credentials.capabilities().inheritable(), cap(CAP_CHOWN));
}

#[def_test]
fn no_setuid_fixup_suppresses_setid_and_setfsuid_capability_changes() {
    let mut credentials = Cred::root();
    credentials.set_securebits(0x4).unwrap();
    credentials.set_uid(1000).unwrap();
    assert_eq!(credentials.capabilities(), CapSets::FULL_PRIVILEGED);

    credentials.set_fsuid(1001);
    assert_eq!(credentials.fsuid(), 1001);
    assert!(credentials.has_cap(CAP_DAC_OVERRIDE));

    credentials.set_securebits(0).unwrap();
    credentials.set_resuid_unchecked(Some(0), Some(0), Some(0));
    credentials.set_fsuid(1000);
    assert_eq!(credentials.fsuid(), 1000);
    assert!(!credentials.has_cap(CAP_DAC_OVERRIDE));
    assert_eq!(credentials.capabilities().permitted(), CapSet::FULL);
    credentials.set_uid(1000).unwrap();
    assert_eq!(
        credentials.capabilities(),
        CapSets::new(CapSet::EMPTY, CapSet::EMPTY, CapSet::EMPTY)
    );
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
fn test_setuid_and_setgid_reject_invalid_ids_before_authorization() {
    // An unprivileged caller would receive EPERM from the authorization
    // check if it ran first, so this pins the EINVAL-before-EPERM order.
    let mut credentials = Cred::new(1000, 100);

    assert_eq!(credentials.set_uid(INVALID_UID), Err(KError::InvalidInput));
    assert_eq!(credentials.set_gid(INVALID_GID), Err(KError::InvalidInput));
    assert_eq!(
        (
            credentials.ruid(),
            credentials.euid(),
            credentials.suid(),
            credentials.fsuid(),
        ),
        (1000, 1000, 1000, 1000)
    );
    assert_eq!(
        (
            credentials.rgid(),
            credentials.egid(),
            credentials.sgid(),
            credentials.fsgid(),
        ),
        (100, 100, 100, 100)
    );
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
fn ptrace_real_credentials_check_requires_target_capability_subset() {
    let caller = credential_with_capabilities(1000, 100, CapSet::EMPTY, cap(CAP_SYS_NICE));
    let target = credential_with_capabilities(1000, 100, CapSet::EMPTY, cap(CAP_SYS_PTRACE));

    assert!(!caller.can_read_ptrace(&target, PtraceAccessMode::RealCreds));
}

#[def_test]
fn ptrace_real_credentials_check_accepts_identity_and_capability_subset() {
    let caller = credential_with_capabilities(1000, 100, CapSet::EMPTY, cap(CAP_SYS_NICE));
    let target = credential_with_capabilities(1000, 100, CapSet::EMPTY, cap(CAP_SYS_NICE));

    assert!(caller.can_read_ptrace(&target, PtraceAccessMode::RealCreds));
}

#[def_test]
fn ptrace_filesystem_credentials_check_uses_effective_capabilities() {
    let caller = credential_with_capabilities(1000, 100, CapSet::EMPTY, cap(CAP_SYS_NICE));
    let target = credential_with_capabilities(1000, 100, CapSet::EMPTY, cap(CAP_SYS_NICE));
    assert!(!caller.can_read_ptrace(&target, PtraceAccessMode::FsCreds));

    let mut caller = caller;
    caller
        .set_capabilities(CapSets::new(
            cap(CAP_SYS_NICE),
            cap(CAP_SYS_NICE),
            CapSet::EMPTY,
        ))
        .unwrap();
    assert!(caller.can_read_ptrace(&target, PtraceAccessMode::FsCreds));
}

#[def_test]
fn ptrace_filesystem_credentials_check_uses_caller_filesystem_ids() {
    let mut caller = Cred::root();
    caller.set_resuid_unchecked(Some(1000), Some(1000), Some(1000));
    caller.set_resgid_unchecked(Some(100), Some(100), Some(100));
    caller.set_fsuid(2000);
    caller.set_capabilities(CapSets::EMPTY).unwrap();
    let target = credential_with_capabilities(1000, 100, CapSet::EMPTY, CapSet::EMPTY);

    assert!(!caller.can_read_ptrace(&target, PtraceAccessMode::FsCreds));
}

#[def_test]
fn ptrace_capability_check_rejects_mismatched_identity_even_with_empty_target() {
    let caller = credential_with_capabilities(1000, 100, CapSet::EMPTY, cap(CAP_SYS_NICE));
    let target = credential_with_capabilities(2000, 100, CapSet::EMPTY, CapSet::EMPTY);

    assert!(!caller.can_read_ptrace(&target, PtraceAccessMode::RealCreds));
}

#[def_test]
fn ptrace_effective_cap_sys_ptrace_overrides_identity_and_subset() {
    let caller = credential_with_capabilities(1000, 100, cap(CAP_SYS_PTRACE), cap(CAP_SYS_PTRACE));
    let target = credential_with_capabilities(2000, 200, CapSet::FULL, CapSet::FULL);

    assert!(caller.can_read_ptrace(&target, PtraceAccessMode::RealCreds));
    assert!(caller.can_read_ptrace(&target, PtraceAccessMode::FsCreds));
}

#[def_test]
fn scheduler_capability_check_requires_target_permitted_subset() {
    let caller = credential_with_capabilities(1000, 100, CapSet::EMPTY, cap(CAP_SYS_NICE));
    let subset_target = credential_with_capabilities(2000, 200, CapSet::EMPTY, cap(CAP_SYS_NICE));
    let privileged_target =
        credential_with_capabilities(2000, 200, CapSet::EMPTY, cap(CAP_SYS_PTRACE));

    assert!(caller.has_scheduler_capability_over(&subset_target));
    assert!(!caller.has_scheduler_capability_over(&privileged_target));
}

#[def_test]
fn effective_cap_sys_nice_overrides_scheduler_target_permitted_subset() {
    let caller = credential_with_capabilities(1000, 100, cap(CAP_SYS_NICE), cap(CAP_SYS_NICE));
    let target = credential_with_capabilities(2000, 200, CapSet::EMPTY, CapSet::FULL);

    assert!(caller.has_scheduler_capability_over(&target));
}

#[def_test]
fn test_initial_credential_is_shared() {
    assert!(Arc::ptr_eq(&initial_cred(), &initial_cred()));
}

#[def_test]
fn capability_bitmap_represents_all_abi_bits() {
    let bits = (1u64 << 63) | (1u64 << (CAP_LAST_CAP + 1)) | (1u64 << CAP_LAST_CAP) | 0b11;
    let set = CapSet::from_words(bits as u32, (bits >> 32) as u32);

    assert_eq!(set.bits(), bits);
    assert_eq!(set.low_word(), bits as u32);
    assert_eq!(set.high_word(), (bits >> 32) as u32);
    assert!(set.contains(CAP_LAST_CAP));
    assert!(!set.contains(CAP_LAST_CAP + 1));
    assert_eq!(set.without(CAP_LAST_CAP + 1).bits(), bits);
}

#[def_test]
fn capability_transition_enforces_linux_subset_rules() {
    let bounding = CapSet::FULL;
    let old = CapSets::new(
        cap(CAP_SETPCAP),
        cap(CAP_SETPCAP) | cap(CAP_CHOWN),
        cap(CAP_CHOWN),
    );

    // Narrowing the inheritable and effective sets is always allowed.
    assert!(
        old.validate_transition(
            &CapSets::new(
                cap(CAP_CHOWN),
                cap(CAP_SETPCAP) | cap(CAP_CHOWN),
                CapSet::EMPTY,
            ),
            bounding,
        )
        .is_ok()
    );

    // Effective must stay inside the new permitted set.
    assert!(
        old.validate_transition(
            &CapSets::new(cap(CAP_CHOWN), cap(CAP_SETPCAP), cap(CAP_CHOWN)),
            bounding,
        )
        .is_err()
    );

    // Effective CAP_SETPCAP relaxes only the old-inheritable/old-permitted
    // bound. The new inheritable set must still fit inside the bounding set.
    assert!(
        old.validate_transition(
            &CapSets::new(
                cap(CAP_CHOWN),
                cap(CAP_SETPCAP) | cap(CAP_CHOWN),
                cap(CAP_SYS_ADMIN),
            ),
            CapSet::FULL,
        )
        .is_ok()
    );
    assert!(
        old.validate_transition(
            &CapSets::new(
                cap(CAP_CHOWN),
                cap(CAP_SETPCAP) | cap(CAP_CHOWN),
                cap(CAP_SYS_ADMIN),
            ),
            CapSet::FULL.without(CAP_SYS_ADMIN),
        )
        .is_err()
    );
}

#[def_test]
fn inheritable_growth_without_setpcap_is_limited_to_old_permitted() {
    let old = CapSets::new(
        cap(CAP_CHOWN),
        cap(CAP_CHOWN) | cap(CAP_DAC_OVERRIDE),
        CapSet::EMPTY,
    );

    assert!(
        old.validate_transition(
            &CapSets::new(
                cap(CAP_CHOWN),
                cap(CAP_CHOWN) | cap(CAP_DAC_OVERRIDE),
                cap(CAP_DAC_OVERRIDE),
            ),
            CapSet::FULL,
        )
        .is_ok()
    );
    assert!(
        old.validate_transition(
            &CapSets::new(
                cap(CAP_CHOWN),
                cap(CAP_CHOWN) | cap(CAP_DAC_OVERRIDE),
                cap(CAP_SYS_ADMIN),
            ),
            CapSet::FULL,
        )
        .is_err()
    );
}

#[def_test]
fn permitted_capabilities_never_grow_through_capset() {
    let mut credentials = Cred::root();
    credentials
        .set_capabilities(CapSets::new(
            cap(CAP_SETPCAP),
            cap(CAP_SETPCAP),
            CapSet::EMPTY,
        ))
        .unwrap();
    let before = credentials.capabilities();

    // Effective CAP_SETPCAP does not turn capset into a way to gain new
    // permitted capabilities.
    assert!(
        credentials
            .set_capabilities(CapSets::new(
                cap(CAP_SETPCAP),
                cap(CAP_SETPCAP) | cap(CAP_SYS_ADMIN),
                CapSet::EMPTY,
            ))
            .is_err()
    );
    assert_eq!(credentials.capabilities(), before);
}

#[def_test]
fn capset_inheritable_growth_is_limited_by_the_bounding_set() {
    let mut credentials = Cred::root();
    credentials
        .drop_bounding_cap(CAP_SYS_ADMIN as usize)
        .unwrap();
    let before = credentials.capabilities();

    assert!(
        credentials
            .set_capabilities(CapSets::new(
                cap(CAP_CHOWN),
                cap(CAP_CHOWN),
                cap(CAP_SYS_ADMIN),
            ))
            .is_err()
    );
    assert_eq!(credentials.capabilities(), before);
}

#[def_test]
fn capset_transition_rejects_effective_outside_permitted_atomically() {
    let mut credentials = Cred::root();
    let before = credentials.capabilities();

    assert!(
        credentials
            .set_capabilities(CapSets::new(cap(CAP_CHOWN), CapSet::EMPTY, CapSet::EMPTY))
            .is_err()
    );
    assert_eq!(credentials.capabilities(), before);
}

#[def_test]
fn dropping_bounding_capability_requires_effective_setpcap_before_range_check() {
    let mut unprivileged = Cred::new(1000, 100);
    assert_eq!(
        unprivileged.drop_bounding_cap(CAP_LAST_CAP as usize + 1),
        Err(KError::OperationNotPermitted)
    );
    assert_eq!(unprivileged.cap_bounding(), CapSet::FULL);

    let mut root = Cred::root();
    assert_eq!(
        root.drop_bounding_cap(CAP_LAST_CAP as usize + 1),
        Err(KError::InvalidInput)
    );
    assert_eq!(
        root.drop_bounding_cap(usize::MAX),
        Err(KError::InvalidInput)
    );
    // A value that would truncate to CAP_CHOWN must still be rejected instead
    // of silently removing the low capability.
    if usize::BITS > 32 {
        let wraps_to_chown = (u32::MAX as usize).wrapping_add(1) + CAP_CHOWN as usize;
        assert_eq!(
            root.drop_bounding_cap(wraps_to_chown),
            Err(KError::InvalidInput)
        );
    }
    assert_eq!(root.cap_bounding(), CapSet::FULL);
    assert!(root.drop_bounding_cap(CAP_CHOWN as usize).is_ok());
    assert!(!root.cap_bounding().contains(CAP_CHOWN));
}

#[def_test]
fn setuid_away_from_root_clears_effective_and_permitted_capabilities() {
    let mut credentials = Cred::root();
    let inheritable = cap(CAP_CHOWN);
    credentials
        .set_capabilities(CapSets::new(CapSet::FULL, CapSet::FULL, inheritable))
        .unwrap();

    credentials.set_uid(1000).unwrap();

    assert_eq!(
        credentials.capabilities(),
        CapSets::new(CapSet::EMPTY, CapSet::EMPTY, inheritable)
    );
    assert_eq!(credentials.cap_bounding(), CapSet::FULL);
}

#[def_test]
fn setuid_away_from_root_with_keep_caps_preserves_permitted_capabilities() {
    let mut credentials = Cred::root();
    credentials.keep_caps_enable().unwrap();

    credentials.set_uid(1000).unwrap();

    assert_eq!(
        credentials.capabilities(),
        CapSets::new(CapSet::EMPTY, CapSet::FULL, CapSet::EMPTY)
    );
}

#[def_test]
fn setuid_to_root_restores_effective_from_permitted_capabilities() {
    let mut credentials = Cred::root();
    credentials
        .set_capabilities(CapSets::new(CapSet::EMPTY, cap(CAP_CHOWN), CapSet::EMPTY))
        .unwrap();
    credentials.set_resuid_unchecked(None, Some(1000), None);

    credentials.set_resuid(None, Some(0), None).unwrap();

    assert_eq!(
        credentials.capabilities(),
        CapSets::new(cap(CAP_CHOWN), cap(CAP_CHOWN), CapSet::EMPTY)
    );
}

#[def_test]
fn leaving_root_as_the_last_root_id_clears_capabilities() {
    let mut credentials = Cred::root();
    credentials.set_resuid_unchecked(None, Some(1000), None);

    credentials
        .set_resuid(Some(1000), None, Some(1000))
        .unwrap();

    assert_eq!(credentials.capabilities(), CapSets::EMPTY);
}

#[def_test]
fn filesystem_uid_fixup_only_changes_effective_filesystem_capabilities() {
    let mut credentials = Cred::root();

    assert_eq!(credentials.set_fsuid(1000), 0);

    assert!(!credentials.has_cap(CAP_DAC_OVERRIDE));
    assert!(credentials.has_cap(CAP_SYS_ADMIN));
    assert_eq!(credentials.capabilities().permitted(), CapSet::FULL);
    assert_eq!(credentials.set_fsuid(0), 1000);
    assert!(credentials.has_cap(CAP_DAC_OVERRIDE));
}

#[def_test]
fn keep_caps_does_not_preserve_filesystem_capabilities_across_setfsuid() {
    let mut credentials = Cred::root();
    credentials.keep_caps_enable().unwrap();

    credentials.set_fsuid(1000);

    assert!(!credentials.has_cap(CAP_DAC_OVERRIDE));
    assert_eq!(credentials.capabilities().permitted(), CapSet::FULL);
}

#[def_test]
fn exec_clears_nonroot_capabilities_but_keeps_inheritable() {
    let mut credentials = Cred::root();
    credentials
        .set_capabilities(CapSets::new(
            cap(CAP_CHOWN),
            cap(CAP_CHOWN),
            cap(CAP_DAC_OVERRIDE),
        ))
        .unwrap();
    credentials.set_resuid_unchecked(Some(1000), Some(1000), Some(1000));
    credentials.keep_caps_enable().unwrap();

    credentials.apply_exec(false, false);

    assert_eq!(
        credentials.capabilities(),
        CapSets::new(CapSet::EMPTY, CapSet::EMPTY, cap(CAP_DAC_OVERRIDE))
    );
    assert_eq!(credentials.cap_bounding(), CapSet::FULL);
    assert!(!credentials.keep_caps());
}

#[def_test]
fn exec_reconstructs_root_capabilities_from_bounding_and_inheritable() {
    let mut credentials = Cred::root();
    credentials
        .drop_bounding_cap(CAP_SYS_ADMIN as usize)
        .unwrap();
    credentials
        .set_capabilities(CapSets::new(
            cap(CAP_CHOWN),
            cap(CAP_CHOWN),
            cap(CAP_DAC_OVERRIDE),
        ))
        .unwrap();

    credentials.apply_exec(false, false);

    let expected_permitted = CapSet::FULL
        .without(CAP_SYS_ADMIN)
        .union(cap(CAP_DAC_OVERRIDE));
    assert_eq!(
        credentials.capabilities(),
        CapSets::new(
            expected_permitted,
            expected_permitted,
            cap(CAP_DAC_OVERRIDE)
        )
    );
    assert_eq!(
        credentials.cap_bounding(),
        CapSet::FULL.without(CAP_SYS_ADMIN)
    );
}

#[def_test]
fn exec_with_real_root_and_nonroot_effective_uid_keeps_permitted_only() {
    let mut credentials = Cred::root();
    credentials.set_resuid_unchecked(Some(0), Some(1000), Some(1000));

    credentials.apply_exec(false, false);

    assert_eq!(
        credentials.capabilities(),
        CapSets::new(CapSet::EMPTY, CapSet::FULL, CapSet::EMPTY)
    );
}

#[def_test]
fn exec_with_secure_noroot_drops_even_root_capabilities() {
    let mut credentials = Cred::root();
    credentials.set_securebits(0x1).unwrap();

    credentials.apply_exec(false, false);

    assert_eq!(credentials.capabilities(), CapSets::EMPTY);
    assert_eq!(credentials.cap_bounding(), CapSet::FULL);
}

#[def_test]
fn exec_with_shared_filesystem_does_not_gain_root_capabilities() {
    let mut credentials = Cred::new(1000, 100);
    credentials.set_resuid_unchecked(None, Some(0), Some(0));

    credentials.apply_exec(false, true);

    assert_eq!(credentials.ruid(), 1000);
    assert_eq!(credentials.euid(), 1000);
    assert_eq!(credentials.suid(), 1000);
    assert_eq!(credentials.fsuid(), 1000);
    assert_eq!(credentials.capabilities(), CapSets::EMPTY);
}

#[def_test]
fn exec_with_shared_filesystem_keeps_effective_id_with_setuid_capability() {
    let mut credentials = Cred::root();
    credentials.set_resuid_unchecked(Some(1000), Some(0), Some(0));
    credentials
        .set_capabilities(CapSets::new(
            cap(CAP_SETUID),
            cap(CAP_SETUID),
            CapSet::EMPTY,
        ))
        .unwrap();

    credentials.apply_exec(false, true);

    assert_eq!(credentials.euid(), 0);
    assert_eq!(credentials.suid(), 0);
    assert_eq!(credentials.fsuid(), 0);
    assert_eq!(
        credentials.capabilities(),
        CapSets::new(cap(CAP_SETUID), cap(CAP_SETUID), CapSet::EMPTY)
    );
}

#[def_test]
fn exec_with_no_new_privs_does_not_regain_dropped_root_capabilities() {
    let mut credentials = Cred::root();
    credentials.set_capabilities(CapSets::EMPTY).unwrap();

    credentials.apply_exec(true, false);

    assert_eq!(credentials.capabilities(), CapSets::EMPTY);
    assert_eq!(credentials.euid(), 0);
}

#[def_test]
fn exec_with_no_new_privs_resets_root_effective_id_and_keeps_old_permitted() {
    let mut credentials = Cred::root();
    credentials.set_resuid_unchecked(Some(0), Some(1000), Some(1000));
    credentials
        .set_capabilities(CapSets::new(CapSet::EMPTY, cap(CAP_CHOWN), CapSet::EMPTY))
        .unwrap();

    credentials.apply_exec(true, false);

    assert_eq!(credentials.euid(), 0);
    assert_eq!(credentials.suid(), 0);
    assert_eq!(credentials.fsuid(), 0);
    assert_eq!(
        credentials.capabilities(),
        CapSets::new(CapSet::EMPTY, cap(CAP_CHOWN), CapSet::EMPTY)
    );
}

#[def_test]
fn exec_with_no_new_privs_preserves_mismatched_ids_without_capability_gain() {
    let mut credentials = Cred::new(1000, 100);
    credentials.set_resuid_unchecked(None, Some(2000), Some(3000));
    credentials.set_resgid_unchecked(None, Some(200), Some(300));

    credentials.apply_exec(true, false);

    // egid 200 equals fsgid 200, so Linux's id_changed group test is false.
    // With no set-ID executable metadata and no permitted growth, the
    // pre-existing euid/real-UID mismatch must therefore be left alone.
    assert_eq!(credentials.ruid(), 1000);
    assert_eq!(credentials.euid(), 2000);
    assert_eq!(credentials.suid(), 2000);
    assert_eq!(credentials.fsuid(), 2000);
    assert_eq!(credentials.rgid(), 100);
    assert_eq!(credentials.egid(), 200);
    assert_eq!(credentials.sgid(), 200);
    assert_eq!(credentials.fsgid(), 200);
    assert_eq!(credentials.capabilities(), CapSets::EMPTY);
}

#[def_test]
fn exec_with_no_new_privs_resets_ids_when_egid_is_outside_the_group_set() {
    let mut credentials = Cred::root();
    credentials.set_resuid_unchecked(Some(0), Some(1000), Some(1000));
    credentials.set_resgid_unchecked(Some(0), Some(0), Some(0));
    assert_eq!(credentials.set_fsgid(1000), 0);

    credentials.apply_exec(true, false);

    // egid 0 is neither fsgid 1000 nor a supplementary group, so Linux's
    // id_changed is true even though this exec changes no IDs itself. The
    // no_new_privs downgrade must reset the effective IDs to the real IDs.
    assert_eq!(credentials.ruid(), 0);
    assert_eq!(credentials.euid(), 0);
    assert_eq!(credentials.suid(), 0);
    assert_eq!(credentials.fsuid(), 0);
    assert_eq!(credentials.rgid(), 0);
    assert_eq!(credentials.egid(), 0);
    assert_eq!(credentials.sgid(), 0);
    assert_eq!(credentials.fsgid(), 0);
    // The prospective euid was non-zero when effective-set selection ran, so
    // resetting it to the real root UID does not retroactively grant pE'.
    assert_eq!(
        credentials.capabilities(),
        CapSets::new(CapSet::EMPTY, CapSet::FULL, CapSet::EMPTY)
    );
}

#[def_test]
fn exec_clears_ambient_when_egid_is_outside_the_group_set() {
    let mut credentials = Cred::root();
    credentials.set_resuid_unchecked(Some(1000), Some(1000), Some(1000));
    credentials.set_resgid_unchecked(Some(100), Some(100), Some(100));
    assert_eq!(credentials.set_fsgid(200), 100);
    credentials
        .set_capabilities(CapSets::new(cap(CAP_CHOWN), cap(CAP_CHOWN), cap(CAP_CHOWN)))
        .unwrap();
    credentials.raise_cap_ambient(CAP_CHOWN).unwrap();

    credentials.apply_exec(false, false);

    // Linux treats this identity change as an ambient-clearing exec even
    // without no_new_privs. The unclamped path otherwise preserves the IDs.
    assert_eq!(credentials.cap_ambient(), CapSet::EMPTY);
    assert_eq!(credentials.euid(), 1000);
    assert_eq!(credentials.egid(), 100);
    assert_eq!(credentials.fsgid(), 100);
    assert_eq!(
        credentials.capabilities(),
        CapSets::new(CapSet::EMPTY, CapSet::EMPTY, cap(CAP_CHOWN))
    );
}

#[def_test]
fn exec_preserves_ambient_when_egid_is_a_supplementary_group() {
    let mut credentials = Cred::root();
    credentials.set_resuid_unchecked(Some(1000), Some(1000), Some(1000));
    credentials.set_resgid_unchecked(Some(100), Some(100), Some(100));
    credentials.set_supplementary_groups(alloc::vec![100]);
    assert_eq!(credentials.set_fsgid(200), 100);
    credentials
        .set_capabilities(CapSets::new(cap(CAP_CHOWN), cap(CAP_CHOWN), cap(CAP_CHOWN)))
        .unwrap();
    credentials.raise_cap_ambient(CAP_CHOWN).unwrap();

    credentials.apply_exec(false, false);

    // egid 100 is a supplementary group even though fsgid is 200, so Linux's
    // id_changed is false and the ambient capability survives.
    assert_eq!(credentials.cap_ambient(), cap(CAP_CHOWN));
    assert_eq!(
        credentials.capabilities(),
        CapSets::new(cap(CAP_CHOWN), cap(CAP_CHOWN), cap(CAP_CHOWN))
    );
    assert_eq!(credentials.egid(), 100);
    assert_eq!(credentials.fsgid(), 100);
}

#[def_test]
fn exec_with_no_new_privs_does_not_promote_root_real_id_to_effective_id() {
    let mut credentials = Cred::root();
    credentials.set_resuid_unchecked(Some(0), Some(1000), Some(1000));

    credentials.apply_exec(true, false);

    // The prospective permitted set is already FULL, so exec gains no
    // capabilities and must not rewrite euid/egid from the real IDs.
    assert_eq!(credentials.euid(), 1000);
    assert_eq!(credentials.suid(), 1000);
    assert_eq!(credentials.fsuid(), 1000);
    assert_eq!(credentials.egid(), 0);
    assert_eq!(credentials.sgid(), 0);
    assert_eq!(credentials.fsgid(), 0);
}

#[def_test]
fn access_credentials_apply_the_linux_effective_set_override() {
    let mut credentials = Cred::root();
    credentials
        .set_capabilities(CapSets::new(cap(CAP_CHOWN), CapSet::FULL, cap(CAP_CHOWN)))
        .unwrap();
    credentials.set_resuid_unchecked(None, Some(1000), None);

    let root_access = credentials.for_access();
    // The real UID is still zero, so the permitted set becomes effective.
    assert_eq!(root_access.capabilities().effective(), CapSet::FULL);
    assert_eq!(root_access.capabilities().permitted(), CapSet::FULL);

    credentials.set_resuid_unchecked(Some(1000), Some(1000), Some(1000));
    let nonroot_access = credentials.for_access();
    assert_eq!(nonroot_access.capabilities().effective(), CapSet::EMPTY);
    assert_eq!(nonroot_access.capabilities().permitted(), CapSet::FULL);
    assert_eq!(nonroot_access.capabilities().inheritable(), cap(CAP_CHOWN));
    assert_eq!(nonroot_access.cap_bounding(), credentials.cap_bounding());

    credentials
        .set_capabilities(CapSets::new(cap(CAP_SETPCAP), CapSet::FULL, cap(CAP_CHOWN)))
        .unwrap();
    credentials.set_securebits(0x4).unwrap();
    let unfixed_access = credentials.for_access();
    assert_eq!(unfixed_access.capabilities(), credentials.capabilities());
}

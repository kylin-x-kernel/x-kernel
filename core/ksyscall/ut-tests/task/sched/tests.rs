// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kcred::Cred;
use kerrno::KError;
use kprocess::NiceValue;
use ktask::{KCpuMask, current};
use unittest::{assert, assert_eq, def_test};

use super::{
    affinity_target, check_scheduler_owner_permission, check_setpriority_permission,
    prepare_setaffinity_target,
};

fn assert_affinity_err<T>(result: Result<T, KError>, expected: KError) {
    match result {
        Err(err) if err == expected => {}
        Err(err) => panic!("expected {expected:?}, got {err:?}"),
        Ok(_) => panic!("expected {expected:?}, got Ok"),
    }
}

#[def_test]
fn getcpu_ignores_legacy_cache_with_null_outputs() {
    for cache in [0, 1, usize::MAX] {
        assert_eq!(
            super::sys_getcpu(
                posix_types::UserPtr::default(),
                posix_types::UserPtr::default(),
                cache,
            ),
            Ok(0)
        );
    }
}

#[def_test]
fn affinity_target_rejects_negative_pid() {
    assert_affinity_err(affinity_target(-1), KError::NoSuchProcess);
    assert_affinity_err(affinity_target(i32::MIN), KError::NoSuchProcess);
}

#[def_test]
fn affinity_target_missing_id_is_no_such_process() {
    // Neither a published tid nor a live tgid.
    assert_affinity_err(affinity_target(i32::MAX), KError::NoSuchProcess);
}

#[def_test(user, serial)]
fn affinity_target_zero_and_current_tid_resolve_caller() {
    let via_zero = affinity_target(0).expect("pid 0 is the caller");
    assert!(current().ptr_eq(&via_zero));

    let tid = kprocess::current_user_tid() as i32;
    let via_tid = affinity_target(tid).expect("caller tid must resolve");
    assert!(current().ptr_eq(&via_tid));
}

#[def_test(user, serial)]
fn setaffinity_missing_pid_precedes_empty_mask_einval() {
    let empty = KCpuMask::new();
    assert_affinity_err(
        prepare_setaffinity_target(i32::MAX, &empty),
        KError::NoSuchProcess,
    );
}

#[def_test(user, serial)]
fn setaffinity_empty_mask_is_einval_after_resolve() {
    let empty = KCpuMask::new();
    assert_affinity_err(prepare_setaffinity_target(0, &empty), KError::InvalidInput);
}

#[def_test]
fn setaffinity_requires_matching_effective_uid() {
    let mut target = Cred::root();
    target
        .set_resuid(Some(1000), Some(2000), Some(1000))
        .unwrap();

    assert!(check_scheduler_owner_permission(&Cred::new(1000, 1), &target).is_ok());
    assert!(check_scheduler_owner_permission(&Cred::new(2000, 1), &target).is_ok());
    assert_eq!(
        check_scheduler_owner_permission(&Cred::new(3000, 1), &target),
        Err(KError::OperationNotPermitted)
    );
    assert!(check_scheduler_owner_permission(&Cred::root(), &target).is_ok());
}

#[def_test]
fn setpriority_requires_matching_effective_uid() {
    let mut target = Cred::root();
    target
        .set_resuid(Some(1000), Some(2000), Some(1000))
        .unwrap();

    assert!(
        check_setpriority_permission(
            &Cred::new(1000, 1),
            &target,
            NiceValue::DEFAULT,
            NiceValue::new_clamped(1),
        )
        .is_ok()
    );
    assert!(
        check_setpriority_permission(
            &Cred::new(2000, 1),
            &target,
            NiceValue::DEFAULT,
            NiceValue::new_clamped(1),
        )
        .is_ok()
    );
    assert_eq!(
        check_setpriority_permission(
            &Cred::new(3000, 1),
            &target,
            NiceValue::DEFAULT,
            NiceValue::new_clamped(1),
        ),
        Err(KError::OperationNotPermitted)
    );
}

#[def_test]
fn setpriority_raise_requires_privilege() {
    let target = Cred::new(1000, 1);

    assert_eq!(
        check_setpriority_permission(
            &target,
            &target,
            NiceValue::new_clamped(5),
            NiceValue::DEFAULT,
        ),
        Err(KError::PermissionDenied)
    );
    assert!(
        check_setpriority_permission(
            &Cred::root(),
            &target,
            NiceValue::new_clamped(5),
            NiceValue::DEFAULT,
        )
        .is_ok()
    );
}

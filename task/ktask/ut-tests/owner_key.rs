// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Owner-key identity split: the dense token and the flag-aligned projection,
//! plus the address fallback for kernel tasks and unknown user tokens.

use alloc::sync::Arc;

use kalloc_types::KBox;
use unittest::{assert_eq, assert_ne, def_test};

use super::TaskInner;

/// A user runtime carrying a real, allocated identity token (unlike the
/// unknown-token runtimes in the task-api tests). Keeps the same task number
/// the caller passes to `TaskInner::new_user`, which asserts the two match.
struct RealTokenRuntime {
    token: kidentity::TaskIdentityToken,
    task_number: Arc<kidentity::PidHandle>,
}

impl RealTokenRuntime {
    fn new(task_number: Arc<kidentity::PidHandle>) -> Self {
        Self {
            token: kidentity::TaskIdentityToken::allocate(),
            task_number,
        }
    }
}

impl crate::UserTaskRuntime for RealTokenRuntime {
    fn identity_token(&self) -> kidentity::TaskIdentityToken {
        self.token
    }

    fn task_number(&self) -> Arc<kidentity::PidHandle> {
        self.task_number.clone()
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

#[def_test]
fn owner_key_aligned_follows_the_identity_split() {
    let task_number = kidentity::allocate_root_pid_handle().expect("test task identity");
    let task = TaskInner::new_user(
        crate::PreparedTaskEntry::new_nowait(|| {}).expect("test entry preparation"),
        "owner-key-token".into(),
        task_number.clone(),
        KBox::new(RealTokenRuntime::new(task_number)).expect("test runtime allocation"),
    );

    let dense = task.owner_key();
    assert_ne!(dense, 0);
    assert_eq!(
        ktask_types::OwnerKey::from_token(dense),
        task.owner_key_aligned()
    );
    assert_eq!(
        task.owner_key_aligned().unwrap().as_raw() & ktask_types::OwnerKey::FLAG_MASK,
        0
    );
}

#[def_test]
fn kernel_identity_address_key_is_returned_unshifted() {
    // The unittest harness runs on kernel tasks: their identity is the
    // `TaskInner` address, which the aligned projection must pass through
    // unchanged with its low flag bits already clear.
    let task = crate::current();
    let dense = task.owner_key();
    assert_ne!(dense, 0);
    assert_eq!(task.owner_key_aligned().unwrap().as_raw(), dense);
    assert_eq!(dense & ktask_types::OwnerKey::FLAG_MASK, 0);
}

#[def_test]
fn unknown_identity_keeps_zero_keys_and_distinct_tokens_stay_distinct() {
    struct UnknownTokenRuntime;

    impl crate::UserTaskRuntime for UnknownTokenRuntime {
        fn as_any(&self) -> &dyn core::any::Any {
            self
        }
    }

    let unknown = TaskInner::new_user(
        crate::PreparedTaskEntry::new_nowait(|| {}).expect("test entry preparation"),
        "owner-key-unknown".into(),
        kidentity::allocate_root_pid_handle().expect("test task identity"),
        KBox::new(UnknownTokenRuntime).expect("test runtime allocation"),
    );
    assert_eq!(unknown.owner_key(), 0);
    assert_eq!(unknown.owner_key_aligned(), None);

    let first_number = kidentity::allocate_root_pid_handle().expect("test task identity");
    let first = TaskInner::new_user(
        crate::PreparedTaskEntry::new_nowait(|| {}).expect("test entry preparation"),
        "owner-key-first".into(),
        first_number.clone(),
        KBox::new(RealTokenRuntime::new(first_number)).expect("test runtime allocation"),
    );
    let second_number = kidentity::allocate_root_pid_handle().expect("test task identity");
    let second = TaskInner::new_user(
        crate::PreparedTaskEntry::new_nowait(|| {}).expect("test entry preparation"),
        "owner-key-second".into(),
        second_number.clone(),
        KBox::new(RealTokenRuntime::new(second_number)).expect("test runtime allocation"),
    );
    assert_ne!(first.owner_key_aligned(), second.owner_key_aligned());
}
// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use unittest::{assert, assert_eq, def_test};

use super::{PidHandle, PidNamespace, root_pid_namespace};

#[def_test]
fn root_pid_handle_starts_at_one() {
    let ns = Arc::new(PidNamespace::new_root());
    let first = PidHandle::allocate_in(&ns).unwrap();
    let second = PidHandle::allocate_in(&ns).unwrap();
    assert_eq!(first.root_nr(), 1);
    assert_eq!(second.root_nr(), 2);
}

#[def_test]
fn child_pid_namespace_gets_pid_one() {
    let root = root_pid_namespace().clone();
    let _root_task = PidHandle::allocate_in(&root).unwrap();
    let child = PidNamespace::new_child(&root);
    let handle = PidHandle::allocate_in(&child).unwrap();

    assert_eq!(handle.nr_in(&child), Some(1));
    assert!(handle.root_nr() >= 2);
}

#[def_test]
fn fixed_root_handle_is_visible_to_any_root_namespace() {
    let other_root = Arc::new(PidNamespace::new_root());
    let handle = PidHandle::fixed_root(42);
    assert_eq!(handle.nr_in(&other_root), Some(42));
}

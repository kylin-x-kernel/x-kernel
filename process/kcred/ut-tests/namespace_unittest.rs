// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, def_test};

use super::*;

#[def_test]
fn test_namespace_id_monotonic() {
    let id1 = NamespaceId::new();
    let id2 = NamespaceId::new();
    let id3 = NamespaceId::new();
    assert!(id2.as_u64() > id1.as_u64());
    assert!(id3.as_u64() > id2.as_u64());
}

#[def_test]
fn test_namespace_id_display() {
    let id = NamespaceId::new();
    let displayed = alloc::format!("{}", id);
    assert!(!displayed.is_empty());
}

#[def_test]
fn test_initial_user_namespace_is_singleton() {
    let first = initial_user_namespace();
    let second = initial_user_namespace();
    assert!(Arc::ptr_eq(&first, &second));
}

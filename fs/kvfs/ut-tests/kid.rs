// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Kernel-side UID/GID identities and user-namespace projection.

use alloc::sync::Arc;

use kcred::{IdMapExtent, UserNamespace, initial_user_namespace};
use unittest::{assert_eq, def_test};

use super::{Kgid, Kuid, OVERFLOW_ID};

/// Builds `parent -> child` with `1000..1001` mapped to child `0..1`.
fn child_namespace() -> Arc<UserNamespace> {
    let parent = initial_user_namespace();
    let namespace = UserNamespace::new_child(&parent, 1000, 1000, false).unwrap();
    let writer = kcred::Cred::new(1000, 1000).with_user_namespace(namespace.clone());
    namespace
        .write_uid_map(&writer, &writer, alloc::vec![IdMapExtent::new(0, 1000, 1).unwrap()])
        .unwrap();
    // The unprivileged GID self-map requires `setgroups=deny` first.
    namespace.set_setgroups_allowed(false).unwrap();
    namespace
        .write_gid_map(&writer, &writer, alloc::vec![IdMapExtent::new(0, 1000, 1).unwrap()])
        .unwrap();
    namespace
}

#[def_test]
fn initial_namespace_projection_is_identity() {
    let initial = initial_user_namespace();
    assert_eq!(Kuid::from_inside(0, &initial).as_raw(), 0);
    assert_eq!(Kuid::from_raw(42).to_inside(&initial), 42);
    assert_eq!(Kgid::from_inside(7, &initial).as_raw(), 7);
    assert_eq!(Kgid::from_raw(7).to_inside(&initial), 7);
}

#[def_test]
fn mapped_ids_translate_in_both_directions() {
    let namespace = child_namespace();
    // Child UID 0 is host UID 1000.
    let kuid = Kuid::from_inside(0, &namespace);
    assert_eq!(kuid.as_raw(), 1000);
    assert_eq!(kuid.to_inside(&namespace), 0);
    // The host view of the same ID is unchanged (global storage semantics).
    assert_eq!(kuid.to_inside(&initial_user_namespace()), 1000);
    // Child UID 5 has no mapping.
    assert!(Kuid::from_inside(5, &namespace).is_invalid());
    // Host UID 42 is not mapped into the child.
    assert!(Kuid::from_inside(42, &namespace).is_invalid());
}

#[def_test]
fn unmapped_ids_render_as_overflow() {
    let namespace = child_namespace();
    assert_eq!(Kuid::from_raw(42).to_inside(&namespace), OVERFLOW_ID);
    assert_eq!(Kgid::from_raw(42).to_inside(&namespace), OVERFLOW_ID);
    assert_eq!(Kuid::INVALID.to_inside(&namespace), OVERFLOW_ID);
    assert_eq!(Kgid::INVALID.to_inside(&namespace), OVERFLOW_ID);
}

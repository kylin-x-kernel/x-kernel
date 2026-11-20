// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kcred::{Cred, IdMapExtent, UserNamespace, initial_user_namespace};
use unittest::{assert_eq, def_test};

use super::{IdMapKind, render_id_map};

#[def_test]
fn id_map_reader_inside_target_sees_parent_coordinates() {
    let initial = initial_user_namespace();
    let child = UserNamespace::new_child(&initial, 1000, 1000, false).unwrap();
    child
        .write_uid_map(&Cred::root(), &Cred::root(), alloc::vec![IdMapExtent::new(0, 1000, 1).unwrap()])
        .unwrap();

    // A reader inside the target namespace must see the parent coordinate
    // (1000), not the target-local coordinate (0). This mirrors Linux
    // `uid_m_show`'s `lower_ns == ns` parent substitution.
    let inside = render_id_map(IdMapKind::Uid, &child, &child);
    assert_eq!(inside.split_whitespace().nth(1), Some("1000"));

    // A reader outside the target sees the same parent coordinate.
    let outside = render_id_map(IdMapKind::Uid, &child, &initial);
    assert_eq!(outside.split_whitespace().nth(1), Some("1000"));

    // Sanity: the inside column stays the target-local start.
    assert_eq!(inside.split_whitespace().next(), Some("0"));
}

#[def_test]
fn id_map_renders_overflow_for_unmapped_reader() {
    let initial = initial_user_namespace();
    let child = UserNamespace::new_child(&initial, 1000, 1000, false).unwrap();
    child
        .write_uid_map(&Cred::root(), &Cred::root(), alloc::vec![IdMapExtent::new(0, 1000, 1).unwrap()])
        .unwrap();
    let sibling = UserNamespace::new_child(&initial, 2000, 2000, false).unwrap();

    // The sibling cannot translate global 1000; Linux `from_kuid` renders
    // (uid_t)-1, not the munged overflow UID 65534.
    let rendered = render_id_map(IdMapKind::Uid, &child, &sibling);
    assert_eq!(
        rendered.split_whitespace().nth(1),
        Some("4294967295")
    );
}

#[def_test]
fn initial_map_is_projected_into_a_child_reader() {
    let initial = initial_user_namespace();
    let reader = UserNamespace::new_child(&initial, 1000, 1000, false).unwrap();
    let rendered = render_id_map(IdMapKind::Uid, &initial, &reader);
    assert_eq!(
        rendered.split_whitespace().nth(1),
        Some("4294967295"),
        "reader does not map global UID 0"
    );
}

#[def_test]
fn id_map_renders_through_a_non_initial_parent() {
    let initial = initial_user_namespace();
    let writer = Cred::root();
    let parent = UserNamespace::new_child(&initial, 1000, 1000, false).unwrap();
    parent
        .write_uid_map(&writer, &writer, alloc::vec![IdMapExtent::new(5, 1000, 1).unwrap()])
        .unwrap();
    parent
        .write_gid_map(&writer, &writer, alloc::vec![IdMapExtent::new(5, 1000, 1).unwrap()])
        .unwrap();
    let child = UserNamespace::new_child(&parent, 1000, 1000, false).unwrap();
    child
        .write_uid_map(&writer, &writer, alloc::vec![IdMapExtent::new(7, 5, 1).unwrap()])
        .unwrap();

    // Parent-local 5 maps to global 1000, so an initial-namespace reader sees
    // 1000 rather than the parent-local value 5.
    let rendered = render_id_map(IdMapKind::Uid, &child, &initial);
    assert_eq!(rendered.split_whitespace().nth(1), Some("1000"));
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use kerrno::LinuxError;
use unittest::{assert, assert_eq, def_test};

use crate::{CapSet, CapSets};

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

#[def_test]
fn id_map_extent_rejects_empty_and_wrapping_ranges() {
    assert_eq!(
        LinuxError::from(IdMapExtent::new(0, 0, 0).unwrap_err()),
        LinuxError::EINVAL
    );
    assert_eq!(
        LinuxError::from(IdMapExtent::new(2, 0, u32::MAX).unwrap_err()),
        LinuxError::EINVAL
    );
    assert_eq!(
        LinuxError::from(IdMapExtent::new(0, 2, u32::MAX).unwrap_err()),
        LinuxError::EINVAL
    );
    assert_eq!(
        LinuxError::from(IdMapExtent::new(u32::MAX, 0, 1).unwrap_err()),
        LinuxError::EINVAL
    );
}

#[def_test]
fn unprivileged_child_can_write_its_own_single_extent_maps() {
    let parent = initial_user_namespace();
    let namespace = UserNamespace::new_child(&parent, 1000, 1000, false).unwrap();
    let writer = Cred::new(1000, 1000).with_user_namespace(namespace.clone());

    assert!(!namespace.is_ready());
    assert_eq!(
        LinuxError::from(namespace.write_uid_map(&writer, &writer, alloc::vec![]).unwrap_err()),
        LinuxError::EINVAL
    );
    assert!(!namespace.has_uid_map());

    namespace
        .write_uid_map(&writer, &writer, alloc::vec![IdMapExtent::new(0, 1000, 1).unwrap()])
        .unwrap();
    assert!(namespace.has_uid_map());
    assert!(!namespace.is_ready());
    assert_eq!(namespace.map_uid_to_initial(0), Some(1000));
    assert_eq!(namespace.map_uid_from_initial(1000), Some(0));

    assert_eq!(
        LinuxError::from(
            namespace
                .write_gid_map(&writer, &writer, alloc::vec![IdMapExtent::new(0, 1000, 1).unwrap()])
                .unwrap_err()
        ),
        LinuxError::EPERM
    );
    namespace.set_setgroups_allowed(false).unwrap();
    namespace
        .write_gid_map(&writer, &writer, alloc::vec![IdMapExtent::new(0, 1000, 1).unwrap()])
        .unwrap();
    assert!(namespace.is_ready());
    assert_eq!(namespace.map_gid_to_initial(0), Some(1000));
}

#[def_test]
fn id_map_is_write_once_per_kind() {
    let parent = initial_user_namespace();
    let namespace = UserNamespace::new_child(&parent, 1000, 1000, false).unwrap();
    let writer = Cred::new(1000, 1000).with_user_namespace(namespace.clone());
    let extent = IdMapExtent::new(0, 1000, 1).unwrap();

    namespace
        .write_uid_map(&writer, &writer, alloc::vec![extent])
        .unwrap();
    assert_eq!(
        LinuxError::from(
            namespace
                .write_uid_map(&writer, &writer, alloc::vec![extent])
                .unwrap_err()
        ),
        LinuxError::EPERM
    );
    assert_eq!(
        LinuxError::from(
            namespace
                .write_gid_map(&writer, &writer, alloc::vec![extent])
                .unwrap_err()
        ),
        LinuxError::EPERM
    );

    namespace.set_setgroups_allowed(false).unwrap();
    namespace
        .write_gid_map(&writer, &writer, alloc::vec![extent])
        .unwrap();
    assert_eq!(
        LinuxError::from(
            namespace
                .write_gid_map(&writer, &writer, alloc::vec![extent])
                .unwrap_err()
        ),
        LinuxError::EPERM
    );
}

#[def_test]
fn id_map_validation_rejects_overlapping_extents() {
    let parent = initial_user_namespace();
    let namespace = UserNamespace::new_child(&parent, 1000, 1000, false).unwrap();
    let writer = Cred::new(1000, 1000).with_user_namespace(namespace.clone());

    assert_eq!(
        LinuxError::from(
            namespace
                .write_uid_map(&writer, &writer,
                    alloc::vec![
                        IdMapExtent::new(0, 1000, 2).unwrap(),
                        IdMapExtent::new(1, 1002, 1).unwrap(),
                    ],
                )
                .unwrap_err()
        ),
        LinuxError::EINVAL
    );
    assert!(!namespace.has_uid_map());
}

#[def_test]
fn setgroups_state_machine_matches_linux() {
    let parent = initial_user_namespace();
    let namespace = UserNamespace::new_child(&parent, 1000, 1000, false).unwrap();

    assert!(namespace.setgroups_allowed());
    namespace.set_setgroups_allowed(false).unwrap();
    assert!(!namespace.setgroups_allowed());
    namespace.set_setgroups_allowed(false).unwrap();
    assert_eq!(
        LinuxError::from(namespace.set_setgroups_allowed(true).unwrap_err()),
        LinuxError::EPERM
    );

    let mapped = UserNamespace::new_child(&parent, 1000, 1000, false).unwrap();
    let writer = Cred::root();
    mapped
        .write_gid_map(&writer, &writer, alloc::vec![IdMapExtent::new(0, 0, 1).unwrap()])
        .unwrap();
    assert_eq!(
        LinuxError::from(mapped.set_setgroups_allowed(false).unwrap_err()),
        LinuxError::EPERM
    );
}

#[def_test]
fn initial_namespace_setgroups_policy_is_immutable() {
    let initial = initial_user_namespace();
    assert!(initial.setgroups_allowed());
    assert_eq!(
        LinuxError::from(initial.set_setgroups_allowed(false).unwrap_err()),
        LinuxError::EPERM
    );
    assert!(initial.setgroups_allowed());
    // `allow` stays idempotent; the fix must not freeze the policy in the
    // permissive direction.
    initial.set_setgroups_allowed(true).unwrap();
}

#[def_test]
fn mapping_host_root_requires_setfcap() {
    let parent = initial_user_namespace();
    let namespace = UserNamespace::new_child(&parent, 1000, 1000, false).unwrap();
    let owner = Cred::new(1000, 1000).with_user_namespace(namespace.clone());
    let root_extent = IdMapExtent::new(0, 0, 1).unwrap();

    assert_eq!(
        LinuxError::from(
            namespace
                .write_uid_map(&owner, &owner, alloc::vec![root_extent])
                .unwrap_err()
        ),
        LinuxError::EPERM
    );
    assert!(!namespace.has_uid_map());

    namespace
        .write_uid_map(&Cred::root(), &Cred::root(), alloc::vec![root_extent])
        .unwrap();
}

#[def_test]
fn mapping_host_root_requires_setfcap_independently() {
    // A writer with CAP_SYS_ADMIN and CAP_SETUID in the parent but without
    // CAP_SETFCAP: without `root_map_is_permitted` the CAP_SETUID fallback
    // would accept the mapping, so this assertion pins the root-map rule.
    let namespace = UserNamespace::new_child(&initial_user_namespace(), 1000, 1000, false)
        .unwrap();
    let mut writer = Cred::root();
    let caps = CapSet::from_bits((1 << CAP_SYS_ADMIN) | (1 << CAP_SETUID));
    writer
        .set_capabilities(CapSets::new(caps, caps, CapSet::EMPTY))
        .unwrap();
    assert_eq!(
        LinuxError::from(
            namespace
                .write_uid_map(&writer, &writer, alloc::vec![IdMapExtent::new(0, 0, 1).unwrap()])
                .unwrap_err()
        ),
        LinuxError::EPERM
    );
    assert!(!namespace.has_uid_map());

    // The same mapping is accepted with CAP_SETFCAP.
    let privileged = UserNamespace::new_child(&initial_user_namespace(), 1000, 1000, false)
        .unwrap();
    privileged
        .write_uid_map(&Cred::root(), &Cred::root(), alloc::vec![IdMapExtent::new(0, 0, 1).unwrap()])
        .unwrap();
}

#[expect(
    clippy::unwrap_used,
    reason = "CG-021-no-panic-on-reachable-failure: test fixture: the parent map is built from \
              constants that the user-namespace API accepts"
)]
fn parent_namespace_with_map() -> Arc<UserNamespace> {
    let parent = UserNamespace::new_child(&initial_user_namespace(), 1000, 1000, false).unwrap();
    let writer = Cred::root();
    parent
        .write_uid_map(&writer, &writer, alloc::vec![IdMapExtent::new(0, 1000, 1).unwrap()])
        .unwrap();
    parent
        .write_gid_map(&writer, &writer, alloc::vec![IdMapExtent::new(0, 1000, 1).unwrap()])
        .unwrap();
    parent
}

#[def_test]
fn nested_map_must_be_covered_by_the_parent_map() {
    let parent = parent_namespace_with_map();
    // The owner ID is in initial-namespace coordinates; the parent maps
    // global 1000 to local 0.
    let namespace = UserNamespace::new_child(&parent, 1000, 1000, false).unwrap();
    let writer = Cred::root();

    assert!(!parent.maps_range_to_parent(MapKind::Uid, 1, 1));
    assert!(parent.maps_range_to_parent(MapKind::Uid, 0, 1));
    assert_eq!(
        LinuxError::from(
            namespace
                .write_uid_map(&writer, &writer, alloc::vec![IdMapExtent::new(0, 1, 1).unwrap()])
                .unwrap_err()
        ),
        LinuxError::EPERM
    );
    namespace
        .write_uid_map(&writer, &writer, alloc::vec![IdMapExtent::new(0, 0, 1).unwrap()])
        .unwrap();
    assert_eq!(namespace.map_uid_to_initial(0), Some(1000));
    assert_eq!(namespace.map_uid_from_initial(1000), Some(0));
}

#[def_test]
fn nested_maps_are_translated_through_every_ancestor() {
    let writer = Cred::root();
    let first = UserNamespace::new_child(&initial_user_namespace(), 5, 5, false).unwrap();
    first
        .write_uid_map(&writer, &writer, alloc::vec![IdMapExtent::new(5, 100, 1).unwrap()])
        .unwrap();
    first
        .write_gid_map(&writer, &writer, alloc::vec![IdMapExtent::new(5, 100, 1).unwrap()])
        .unwrap();

    // The owner is global 100, which `first` maps to local 5.
    let second = UserNamespace::new_child(&first, 100, 100, false).unwrap();
    second
        .write_uid_map(&writer, &writer, alloc::vec![IdMapExtent::new(7, 5, 1).unwrap()])
        .unwrap();
    second
        .write_gid_map(&writer, &writer, alloc::vec![IdMapExtent::new(7, 5, 1).unwrap()])
        .unwrap();

    let third = UserNamespace::new_child(&second, 100, 100, false).unwrap();
    assert!(second.maps_range_to_parent(MapKind::Uid, 7, 1));
    assert!(!second.maps_range_to_parent(MapKind::Uid, 8, 1));
    assert_eq!(
        LinuxError::from(
            third
                .write_uid_map(&writer, &writer, alloc::vec![IdMapExtent::new(9, 8, 1).unwrap()])
                .unwrap_err()
        ),
        LinuxError::EPERM
    );

    third
        .write_uid_map(&writer, &writer, alloc::vec![IdMapExtent::new(9, 7, 1).unwrap()])
        .unwrap();
    assert!(third.maps_range_to_parent(MapKind::Uid, 9, 1));
    assert!(!third.maps_range_to_parent(MapKind::Uid, 8, 1));
    assert!(!third.maps_range_to_parent(MapKind::Uid, 9, 2));
    assert_eq!(third.map_uid_to_initial(9), Some(100));
    assert_eq!(third.map_uid_from_initial(100), Some(9));
}

#[def_test]
fn namespace_owner_administers_its_direct_child() {
    let initial = initial_user_namespace();
    // The owner of a child namespace has all capabilities over it when acting
    // from the direct parent (Linux `cap_capable` rule 1).
    let child = UserNamespace::new_child(&initial, 1000, 1000, false).unwrap();
    let owner = Cred::new(1000, 1000);
    assert!(owner.has_cap_in(&child, CAP_SYS_ADMIN));
    assert!(owner.has_cap_in(&child, CAP_SETUID));
    // A non-owner with the same numeric ID in the parent has no such grant.
    let other = Cred::new(1001, 1001);
    assert!(!other.has_cap_in(&child, CAP_SYS_ADMIN));
    // Root of the parent still reaches the child through its effective set.
    assert!(Cred::root().has_cap_in(&child, CAP_SYS_ADMIN));
}

#[def_test]
fn nested_owner_can_write_its_own_single_extent_map() {
    let writer = Cred::root();
    let parent = UserNamespace::new_child(&initial_user_namespace(), 1000, 1000, false).unwrap();
    parent
        .write_uid_map(&writer, &writer, alloc::vec![IdMapExtent::new(5, 1000, 1).unwrap()])
        .unwrap();
    parent
        .write_gid_map(&writer, &writer, alloc::vec![IdMapExtent::new(5, 1000, 1).unwrap()])
        .unwrap();

    let child = UserNamespace::new_child(&parent, 1000, 1000, false).unwrap();
    let child_owner = Cred::new(1000, 1000).with_user_namespace(child.clone());
    child
        .write_uid_map(&child_owner, &child_owner,
            alloc::vec![IdMapExtent::new(7, 5, 1).unwrap()],
        )
        .unwrap();
    assert_eq!(child.map_uid_to_initial(7), Some(1000));
}

#[def_test]
fn map_write_requires_the_writing_task_parent_capability() {
    let namespace = UserNamespace::new_child(&initial_user_namespace(), 1000, 1000, false)
        .unwrap();
    let opener = Cred::root();
    let current = Cred::new(1000, 1000);
    assert_eq!(
        LinuxError::from(
            namespace
                .write_uid_map(
                    &opener,
                    &current,
                    alloc::vec![IdMapExtent::new(0, 0, 1).unwrap()],
                )
                .unwrap_err()
        ),
        LinuxError::EPERM
    );
}

#[def_test]
fn capabilities_never_flow_toward_ancestors() {
    let initial = initial_user_namespace();
    let child = UserNamespace::new_child(&initial, 1000, 1000, false).unwrap();
    let writer = Cred::root();
    child
        .write_uid_map(&writer, &writer, alloc::vec![IdMapExtent::new(0, 1000, 1).unwrap()])
        .unwrap();
    child
        .write_gid_map(&writer, &writer, alloc::vec![IdMapExtent::new(0, 1000, 1).unwrap()])
        .unwrap();
    let grandchild = UserNamespace::new_child(&child, 1000, 1000, false).unwrap();

    // A credential in the child namespace cannot administer the parent, even
    // though the child namespace is fully privileged inside itself.
    let child_root = Cred::root_in_user_namespace(child.clone());
    assert!(!child_root.has_cap_in(&initial, CAP_SYS_ADMIN));

    // Nor can it administer an unrelated sibling.
    let sibling = UserNamespace::new_child(&initial, 2000, 2000, false).unwrap();
    assert!(!child_root.has_cap_in(&sibling, CAP_SYS_ADMIN));

    // It does hold authority over its own descendants.
    assert!(child_root.has_cap_in(&grandchild, CAP_SYS_ADMIN));
}

#[def_test]
fn grandchild_owner_grant_requires_the_direct_parent() {
    let initial = initial_user_namespace();
    let child = UserNamespace::new_child(&initial, 1000, 1000, false).unwrap();
    let writer = Cred::root();
    child
        .write_uid_map(&writer, &writer, alloc::vec![IdMapExtent::new(0, 1000, 1).unwrap()])
        .unwrap();
    child
        .write_gid_map(&writer, &writer, alloc::vec![IdMapExtent::new(0, 1000, 1).unwrap()])
        .unwrap();
    // Global UID 1000 (local 0 in the child) creates the grandchild.
    let grandchild = UserNamespace::new_child(&child, 1000, 1000, false).unwrap();

    // The grandchild's owner grant is evaluated from its direct parent (the
    // child namespace), where the owner's credential lives.
    let child_root = Cred::root_in_user_namespace(child.clone());
    assert!(child_root.has_cap_in(&grandchild, CAP_SYS_ADMIN));

    // The grandchild's indirect owner (UID 1000 created the child in the
    // initial namespace) still reaches it: Linux applies the owner rule at
    // the child level while walking up, which covers all descendants.
    assert!(Cred::new(1000, 1000).has_cap_in(&grandchild, CAP_SYS_ADMIN));
    // An unrelated ID in the initial namespace has no grant.
    let unprivileged = Cred::new(2000, 2000);
    assert!(!unprivileged.has_cap_in(&grandchild, CAP_SYS_ADMIN));
}

#[def_test]
fn namespace_nesting_is_capped_at_linux_depth() {
    let mut namespace = initial_user_namespace();
    let writer = Cred::root();
    for _ in 0..33 {
        let child = UserNamespace::new_child(&namespace, 0, 0, false).unwrap();
        child
            .write_uid_map(&writer, &writer, alloc::vec![IdMapExtent::new(0, 0, 1).unwrap()])
            .unwrap();
        child
            .write_gid_map(&writer, &writer, alloc::vec![IdMapExtent::new(0, 0, 1).unwrap()])
            .unwrap();
        namespace = child;
    }
    // Linux rejects a child of a level-33 namespace (`parent->level > 32`).
    assert_eq!(
        LinuxError::from(
            UserNamespace::new_child(&namespace, 0, 0, false)
                .unwrap_err()
        ),
        LinuxError::ENOSPC
    );
}

#[def_test]
fn gid_self_map_requires_the_namespace_owner() {
    let namespace =
        UserNamespace::new_child(&initial_user_namespace(), 1000, 1000, false).unwrap();
    let root = Cred::root();
    namespace.set_setgroups_allowed(false).unwrap();
    let mut attacker = Cred::root();
    attacker
        .set_resgid(Some(1001), Some(1001), Some(1001))
        .unwrap();
    let sys_admin = CapSet::from_bits(1 << CAP_SYS_ADMIN);
    attacker
        .set_capabilities(CapSets::new(sys_admin, CapSet::FULL, CapSet::EMPTY))
        .unwrap();
    assert_eq!(
        LinuxError::from(
            namespace
                .write_gid_map(
                    &attacker,
                    &attacker,
                    alloc::vec![IdMapExtent::new(0, 1001, 1).unwrap()],
                )
                .unwrap_err()
        ),
        LinuxError::EPERM
    );
    // The owner still installs its own single extent.
    namespace
        .write_gid_map(&root, &root, alloc::vec![IdMapExtent::new(0, 1000, 1).unwrap()])
        .unwrap();
}

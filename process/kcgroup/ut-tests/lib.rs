// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

#[def_test]
fn initial_namespace_is_shared() {
    assert!(Arc::ptr_eq(
        &CgroupNamespace::initial(),
        &CgroupNamespace::initial()
    ));
}

fn make_pids_group(name: &str) -> (Arc<Cgroup>, Arc<Cgroup>) {
    let root = Cgroup::new_root();
    root.set_pids_subtree_enabled(true).unwrap();
    let child = root.create_child(name).unwrap();
    (root, child)
}

#[def_test]
fn failed_reservation_does_not_leak_a_charge() {
    let (_root, group) = make_pids_group("limited");
    group.set_pids_max(Some(1)).unwrap();
    let membership = group
        .reserve_task()
        .unwrap()
        .commit(kidentity::TaskIdentityToken::allocate())
        .unwrap();
    assert_eq!(
        group.reserve_task().err(),
        Some(KError::from(LinuxError::EAGAIN))
    );
    assert_eq!(group.pids_current(), Ok(1));
    drop(membership);
    assert_eq!(group.pids_current(), Ok(0));
}

#[def_test]
fn migration_ignores_target_limit_and_transfers_charge() {
    let root = Cgroup::new_root();
    root.set_pids_subtree_enabled(true).unwrap();
    let source = root.create_child("source").unwrap();
    let target = root.create_child("target").unwrap();
    target.set_pids_max(Some(0)).unwrap();
    let membership = source
        .reserve_task()
        .unwrap()
        .commit(kidentity::TaskIdentityToken::allocate())
        .unwrap();
    membership.migrate(&target).unwrap();
    assert_eq!(source.pids_current(), Ok(0));
    assert_eq!(target.pids_current(), Ok(1));
    assert_eq!(membership.cgroup().unwrap().path(), "/target");
}

#[def_test]
fn lowering_limit_below_current_is_allowed() {
    let (_root, group) = make_pids_group("limited");
    let _first = group
        .reserve_task()
        .unwrap()
        .commit(kidentity::TaskIdentityToken::allocate())
        .unwrap();
    let _second = group
        .reserve_task()
        .unwrap()
        .commit(kidentity::TaskIdentityToken::allocate())
        .unwrap();
    group.set_pids_max(Some(1)).unwrap();
    assert_eq!(group.pids_current(), Ok(2));
    assert!(group.reserve_task().is_err());
}

#[def_test]
fn pids_limit_rejects_values_outside_the_linux_pid_domain() {
    let (_root, group) = make_pids_group("bounded");

    assert_eq!(group.set_pids_max(Some(Cgroup::PIDS_MAX_LIMIT)), Ok(()));
    assert_eq!(group.pids_max(), Ok(Some(Cgroup::PIDS_MAX_LIMIT)));
    assert_eq!(
        group.set_pids_max(Some(Cgroup::PIDS_MAX_LIMIT + 1)),
        Err(KError::from(LinuxError::EINVAL))
    );
    assert_eq!(group.pids_max(), Ok(Some(Cgroup::PIDS_MAX_LIMIT)));
}

#[def_test]
fn operation_guard_blocks_removal_and_old_handles_fail_after_removal() {
    let (root, child) = make_pids_group("guarded");
    let operation = child.begin_operation().unwrap();

    assert_eq!(
        root.remove_child("guarded"),
        Err(KError::from(LinuxError::EBUSY))
    );
    drop(operation);
    root.remove_child("guarded").unwrap();

    assert_eq!(
        child.begin_operation().err(),
        Some(KError::from(LinuxError::ENODEV))
    );
    assert_eq!(
        child.set_pids_max(Some(1)),
        Err(KError::from(LinuxError::ENODEV))
    );
}

#[def_test]
fn subtree_visibility_and_common_ancestor_use_stable_node_identity() {
    let root = Cgroup::new_root();
    let delegated = root.create_child("delegated").unwrap();
    let left = delegated.create_child("left").unwrap();
    let right = delegated.create_child("right").unwrap();
    let outside = root.create_child("outside").unwrap();

    assert!(left.is_descendant_of(&delegated));
    assert!(delegated.is_descendant_of(&delegated));
    assert!(!outside.is_descendant_of(&delegated));
    assert!(Arc::ptr_eq(
        &left.common_ancestor(&right).unwrap(),
        &delegated
    ));
    assert!(Arc::ptr_eq(&left.common_ancestor(&outside).unwrap(), &root));
    assert_eq!(
        left.common_ancestor(&Cgroup::new_root()).err(),
        Some(KError::from(LinuxError::EXDEV))
    );
}

#[def_test]
fn namespace_paths_are_relative_to_the_view_root() {
    let root = Cgroup::new_root();
    let left = root.create_child("left").unwrap();
    let nested = left.create_child("nested").unwrap();
    let right = root.create_child("right").unwrap();

    assert_eq!(left.path_from(&left), Ok("/".to_string()));
    assert_eq!(nested.path_from(&left), Ok("/nested".to_string()));
    assert_eq!(right.path_from(&left), Ok("/../right".to_string()));
}

#[def_test]
fn namespace_path_rejects_another_hierarchy() {
    let left = CgroupNamespace::new().root();
    let right = CgroupNamespace::new().root();

    assert_eq!(left.path_from(&right), Err(KError::from(LinuxError::EXDEV)));
}

#[def_test]
fn duplicate_identity_commit_preserves_membership_and_rolls_back_charge() {
    let (root, group) = make_pids_group("duplicate");
    let token = kidentity::TaskIdentityToken::allocate();
    let membership = group.reserve_task().unwrap().commit(token).unwrap();

    assert_eq!(
        group.reserve_task().unwrap().commit(token).err(),
        Some(KError::from(LinuxError::EEXIST))
    );
    assert_eq!(group.pids_current(), Ok(1));
    let members = group.member_tasks();
    assert_eq!(members.len(), 1);
    assert_eq!(members[0], token);
    assert_eq!(
        root.remove_child("duplicate"),
        Err(KError::from(LinuxError::EBUSY))
    );

    drop(membership);
    assert_eq!(group.pids_current(), Ok(0));
    root.remove_child("duplicate").unwrap();
}

#[def_test]
fn distinct_identity_tokens_keep_distinct_memberships() {
    let (_root, group) = make_pids_group("reused");
    let first_token = kidentity::TaskIdentityToken::allocate();
    let second_token = kidentity::TaskIdentityToken::allocate();
    let first = group.reserve_task().unwrap().commit(first_token).unwrap();
    let second = group.reserve_task().unwrap().commit(second_token).unwrap();

    let members = group.member_tasks();
    assert_eq!(members.len(), 2);
    assert!(members.contains(&first_token));
    assert!(members.contains(&second_token));

    drop(first);
    drop(second);
    assert_eq!(group.pids_current(), Ok(0));
}

#[def_test]
fn removed_cgroup_rejects_reservation_and_migration() {
    let root = Cgroup::new_root();
    let removed = root.create_child("removed").unwrap();
    root.remove_child("removed").unwrap();

    assert_eq!(
        removed.reserve_task().err(),
        Some(KError::from(LinuxError::ENOENT))
    );
    let token = kidentity::TaskIdentityToken::allocate();
    let membership = root.reserve_task().unwrap().commit(token).unwrap();
    assert_eq!(
        membership.migrate(&removed).err(),
        Some(KError::from(LinuxError::ENOENT))
    );
    assert!(Arc::ptr_eq(&membership.cgroup().unwrap(), &root));
    assert!(root.member_tasks().contains(&token));
    assert!(!removed.has_pids_controller());
}

#[def_test]
fn failed_busy_removal_keeps_cgroup_active() {
    let root = Cgroup::new_root();
    let child = root.create_child("busy").unwrap();
    let membership = child
        .reserve_task()
        .unwrap()
        .commit(kidentity::TaskIdentityToken::allocate())
        .unwrap();

    assert_eq!(
        root.remove_child("busy").err(),
        Some(KError::from(LinuxError::EBUSY))
    );
    assert!(child.reserve_task().is_ok());
    drop(membership);
}

#[def_test]
fn detached_membership_is_observable_without_panicking() {
    let (_root, group) = make_pids_group("member");
    let membership = group
        .reserve_task()
        .unwrap()
        .commit(kidentity::TaskIdentityToken::allocate())
        .unwrap();

    membership.detach();
    membership.detach();
    assert!(membership.cgroup().is_none());
    assert_eq!(group.pids_current(), Ok(0));
}

#[def_test]
fn pids_controller_is_not_exposed_on_the_hierarchy_root() {
    let root = Cgroup::new_root();

    assert!(!root.has_pids_controller());
    assert!(root.has_available_pids_controller());
    assert_eq!(root.pids_current(), Err(KError::from(LinuxError::ENOENT)));
}

#[def_test]
fn subtree_control_activates_controller_for_direct_children() {
    let root = Cgroup::new_root();
    let child = root.create_child("child").unwrap();
    let membership = child
        .reserve_task()
        .unwrap()
        .commit(kidentity::TaskIdentityToken::allocate())
        .unwrap();

    root.set_pids_subtree_enabled(true).unwrap();
    assert!(child.has_pids_controller());
    assert_eq!(child.pids_current(), Ok(1));
    drop(membership);
    assert_eq!(child.pids_current(), Ok(0));
}

#[def_test]
fn non_root_internal_process_blocks_controller_delegation() {
    let (_root, child) = make_pids_group("child");
    let _membership = child
        .reserve_task()
        .unwrap()
        .commit(kidentity::TaskIdentityToken::allocate())
        .unwrap();

    assert_eq!(
        child.set_pids_subtree_enabled(true),
        Err(KError::from(LinuxError::EBUSY))
    );
}

#[def_test]
fn outstanding_reservation_blocks_removal_until_commit_or_drop() {
    let (root, child) = make_pids_group("reserved");
    let charge = child.reserve_task().unwrap();
    assert_eq!(
        root.remove_child("reserved"),
        Err(KError::from(LinuxError::EBUSY))
    );
    drop(charge);
    root.remove_child("reserved").unwrap();
}

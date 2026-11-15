// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{string::ToString, sync::Arc};

use kerrno::{KError, LinuxError};
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
fn pids_events_count_limit_rejections_but_not_migration() {
    let root = Cgroup::new_root();
    root.set_pids_subtree_enabled(true).unwrap();
    let source = root.create_child("source").unwrap();
    let target = root.create_child("target").unwrap();
    target.set_pids_max(Some(0)).unwrap();
    assert_eq!(target.pids_max_hits(), Ok(0));

    let membership = source
        .reserve_task()
        .unwrap()
        .commit(kidentity::TaskIdentityToken::allocate())
        .unwrap();
    membership.migrate(&target).unwrap();
    assert_eq!(target.pids_max_hits(), Ok(0));

    assert_eq!(
        target.reserve_task().err(),
        Some(KError::from(LinuxError::EAGAIN))
    );
    assert_eq!(target.pids_max_hits(), Ok(1));
    assert_eq!(source.pids_max_hits(), Ok(0));
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
fn stale_child_handle_cannot_remove_a_recreated_cgroup() {
    let root = Cgroup::new_root();
    let stale = root.create_child("reused").unwrap();
    root.remove_child("reused").unwrap();
    let current = root.create_child("reused").unwrap();

    assert_eq!(
        root.remove_child_node(&stale).err(),
        Some(KError::from(LinuxError::ENOENT))
    );
    assert!(current.is_active());
    assert!(Arc::ptr_eq(&root.child("reused").unwrap(), &current));
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
fn non_root_delegation_allows_tasks_in_descendants() {
    let (_root, child) = make_pids_group("parent");
    let grandchild = child.create_child("grandchild").unwrap();
    let membership = grandchild
        .reserve_task()
        .unwrap()
        .commit(kidentity::TaskIdentityToken::allocate())
        .unwrap();

    child.set_pids_subtree_enabled(true).unwrap();
    assert_eq!(child.pids_current(), Ok(1));
    assert!(grandchild.has_pids_controller());
    assert_eq!(grandchild.pids_current(), Ok(1));

    drop(membership);
    assert_eq!(child.pids_current(), Ok(0));
}

#[def_test]
fn disabling_subtree_control_keeps_the_current_controller_live() {
    let (root, child) = make_pids_group("parent");
    child.set_pids_subtree_enabled(true).unwrap();
    let grandchild = child.create_child("grandchild").unwrap();
    assert!(grandchild.has_pids_controller());

    child.set_pids_subtree_enabled(false).unwrap();
    assert!(child.has_pids_controller());
    assert_eq!(child.pids_current(), Ok(0));
    assert!(!grandchild.has_pids_controller());
    assert!(root.pids_subtree_enabled());
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

fn cpus(list: &str) -> CpuMask {
    CpuMask::parse_list(list).unwrap()
}

fn mems(list: &str) -> NodeMask {
    NodeMask::parse_list(list).unwrap()
}

#[def_test]
fn cpuset_list_parser_round_trips_linux_syntax() {
    assert_eq!(cpus("0-2,5,7-8").format_list(), "0-2,5,7-8");
    assert_eq!(cpus(" 3, 1-2 ").format_list(), "1-3");
    assert_eq!(cpus("5").format_list(), "5");
    assert!(cpus("").is_empty());
    assert!(cpus(" \t ").is_empty());
    assert_eq!(mems("0-1,3").format_list(), "0-1,3");

    for malformed in ["0,,1", "1-0", "0-", "-1", "0-1-2", "a", "0-1,"] {
        assert_eq!(
            CpuMask::parse_list(malformed),
            Err(KError::from(LinuxError::EINVAL)),
            "accepted malformed cpulist {malformed:?}"
        );
    }
    assert_eq!(
        CpuMask::parse_list("1024"),
        Err(KError::from(LinuxError::EINVAL))
    );
    assert_eq!(
        NodeMask::parse_list("1024"),
        Err(KError::from(LinuxError::EINVAL))
    );
}

#[def_test]
fn cpuset_root_starts_with_an_inherited_empty_request() {
    let root = Cgroup::new_root_with_universe(cpus("0-3"), mems("0-1"));

    assert_eq!(root.cpuset_cpus(), Ok(CpuMask::empty()));
    assert_eq!(root.cpuset_mems(), Ok(NodeMask::empty()));
    assert_eq!(root.cpuset_effective_cpus(), Ok(cpus("0-3")));
    assert_eq!(root.cpuset_effective_mems(), Ok(mems("0-1")));
    assert!(!root.cpuset_subtree_enabled());

    root.set_cpuset_cpus(cpus("1-3")).unwrap();
    assert_eq!(root.cpuset_cpus(), Ok(cpus("1-3")));
    assert_eq!(root.cpuset_effective_cpus(), Ok(cpus("1-3")));
    root.set_cpuset_cpus(CpuMask::empty()).unwrap();
    assert_eq!(root.cpuset_effective_cpus(), Ok(cpus("0-3")));
}

#[def_test]
fn cpuset_child_activation_follows_parent_subtree_control() {
    let root = Cgroup::new_root_with_universe(cpus("0-3"), mems("0-1"));
    let child = root.create_child("child").unwrap();

    assert_eq!(
        child.cpuset_effective_cpus(),
        Err(KError::from(LinuxError::ENOENT))
    );
    root.set_cpuset_subtree_enabled(true).unwrap();
    assert_eq!(child.cpuset_effective_cpus(), Ok(cpus("0-3")));
    assert_eq!(child.cpuset_effective_mems(), Ok(mems("0-1")));

    child.set_cpuset_subtree_enabled(true).unwrap();
    child.set_cpuset_subtree_enabled(false).unwrap();
    assert_eq!(child.cpuset_effective_cpus(), Ok(cpus("0-3")));

    assert_eq!(
        root.set_subtree_control(ControllerKind::Cpuset, true),
        Ok(false)
    );
    assert_eq!(
        root.set_subtree_control(ControllerKind::Cpuset, false),
        Ok(true)
    );
    assert_eq!(
        root.set_subtree_control(ControllerKind::Cpuset, false),
        Ok(false)
    );
    assert_eq!(
        child.cpuset_effective_cpus(),
        Err(KError::from(LinuxError::ENOENT))
    );
}

#[def_test]
fn cpuset_masks_inherit_and_refresh_through_the_subtree() {
    let root = Cgroup::new_root_with_universe(cpus("0-3"), mems("0-1"));
    root.set_cpuset_subtree_enabled(true).unwrap();
    let child = root.create_child("child").unwrap();
    child.set_cpuset_subtree_enabled(true).unwrap();
    let grandchild = child.create_child("grandchild").unwrap();

    child.set_cpuset_cpus(cpus("1-2")).unwrap();
    child.set_cpuset_mems(mems("1")).unwrap();
    assert_eq!(child.cpuset_effective_cpus(), Ok(cpus("1-2")));
    assert_eq!(grandchild.cpuset_cpus(), Ok(CpuMask::empty()));
    assert_eq!(grandchild.cpuset_effective_cpus(), Ok(cpus("1-2")));
    assert_eq!(grandchild.cpuset_effective_mems(), Ok(mems("1")));

    root.set_cpuset_cpus(cpus("2-3")).unwrap();
    assert_eq!(child.cpuset_cpus(), Ok(cpus("1-2")));
    assert_eq!(child.cpuset_effective_cpus(), Ok(cpus("2")));
    assert_eq!(grandchild.cpuset_effective_cpus(), Ok(cpus("2")));

    root.set_cpuset_cpus(cpus("3")).unwrap();
    assert!(child.cpuset_effective_cpus().unwrap().is_empty());
    assert!(grandchild.cpuset_effective_cpus().unwrap().is_empty());
    assert_eq!(
        grandchild.set_cpuset_cpus(CpuMask::empty()),
        Err(KError::from(LinuxError::ENOSPC))
    );

    root.set_cpuset_cpus(cpus("0-3")).unwrap();
    assert_eq!(child.cpuset_effective_cpus(), Ok(cpus("1-2")));
    assert_eq!(grandchild.cpuset_effective_cpus(), Ok(cpus("1-2")));
}

#[def_test]
fn cpuset_rejects_cpus_outside_the_universe_or_parent_effective_mask() {
    let root = Cgroup::new_root_with_universe(cpus("0-3"), mems("0-1"));
    root.set_cpuset_subtree_enabled(true).unwrap();
    let child = root.create_child("child").unwrap();

    assert_eq!(
        root.set_cpuset_cpus(cpus("4")),
        Err(KError::from(LinuxError::ENODEV))
    );
    assert_eq!(
        child.set_cpuset_cpus(cpus("4")),
        Err(KError::from(LinuxError::ENODEV))
    );
    assert_eq!(
        root.set_cpuset_mems(mems("2")),
        Err(KError::from(LinuxError::ENODEV))
    );

    root.set_cpuset_cpus(cpus("0-1")).unwrap();
    assert_eq!(
        child.set_cpuset_cpus(cpus("2")),
        Err(KError::from(LinuxError::EINVAL))
    );

    root.set_cpuset_mems(mems("0")).unwrap();
    assert_eq!(
        child.set_cpuset_mems(mems("1")),
        Err(KError::from(LinuxError::EINVAL))
    );
}

#[def_test]
fn cpuset_delegation_rejects_a_non_root_with_direct_members() {
    let root = Cgroup::new_root_with_universe(cpus("0-3"), mems("0-1"));
    let child = root.create_child("child").unwrap();
    let membership = child
        .reserve_task()
        .unwrap()
        .commit(kidentity::TaskIdentityToken::allocate())
        .unwrap();

    root.set_cpuset_subtree_enabled(true).unwrap();
    assert_eq!(
        child.set_cpuset_subtree_enabled(true),
        Err(KError::from(LinuxError::EBUSY))
    );

    drop(membership);
    child.set_cpuset_subtree_enabled(true).unwrap();
}

#[def_test]
fn cpu_controller_applies_weight_and_bandwidth_rules() {
    let root = Cgroup::new_root();
    root.set_subtree_control(ControllerKind::Cpu, true).unwrap();
    let child = root.create_child("cpu-test").unwrap();

    // Linux defaults.
    let controller = child.cpu_controller().unwrap();
    assert_eq!(controller.weight(), 100);
    assert_eq!(controller.quota(), None);
    assert_eq!(controller.period(), 100_000);

    // Weight accepts the Linux range and rejects the edges outside it.
    controller.set_weight(10_000).unwrap();
    assert_eq!(controller.weight(), 10_000);
    assert_eq!(controller.set_weight(0).err(), Some(KError::from(LinuxError::ERANGE)));
    assert_eq!(
        controller.set_weight(10_001).err(),
        Some(KError::from(LinuxError::ERANGE))
    );

    // Bandwidth: numeric quota with a valid period, then Linux's edges.
    controller.set_bandwidth(Some(200_000), 100_000).unwrap();
    assert_eq!(controller.quota(), Some(200_000));
    assert_eq!(
        controller.set_bandwidth(Some(999), 100_000).err(),
        Some(KError::from(LinuxError::EINVAL))
    );
    assert_eq!(
        controller.set_bandwidth(Some(200_000), 999).err(),
        Some(KError::from(LinuxError::EINVAL))
    );
    assert_eq!(
        controller.set_bandwidth(Some(200_000), 1_000_001).err(),
        Some(KError::from(LinuxError::EINVAL))
    );
    // "max" resets the quota.
    controller.set_bandwidth(None, 100_000).unwrap();
    assert_eq!(controller.quota(), None);
}

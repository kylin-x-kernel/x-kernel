// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{sync::Arc, vec::Vec};

use kcgroup::{CgroupNamespace, ControllerKind};
use kvfs::{Filename, Mount, NodePermission, Path, SimpleFs, SuperBlockFlags, VfsError};
use unittest::{assert_eq, def_test};

use super::command;

#[def_test]
fn command_parser_rejects_embedded_nul() {
    assert_eq!(
        command::parse_command(b"+pids\0-pids"),
        Err(VfsError::InvalidInput)
    );
}

#[def_test]
fn subtree_parser_rejects_empty_and_malformed_operations() {
    assert_eq!(
        command::parse_subtree_control(b""),
        Err(VfsError::InvalidInput)
    );
    assert_eq!(
        command::parse_subtree_control(b"pids"),
        Err(VfsError::InvalidInput)
    );
    assert_eq!(
        command::parse_subtree_control(b"+"),
        Err(VfsError::InvalidInput)
    );
}

#[def_test]
fn mount_state_reuses_nodes_and_distinguishes_recreated_cgroups() {
    let root = CgroupNamespace::new().root();
    let first_group = root.create_child("child").unwrap();
    let super_block = crate::new_cgroup2fs(root.clone(), SuperBlockFlags::empty()).unwrap();
    let fs = super_block.private::<Arc<SimpleFs>>().unwrap();
    let state = fs.private::<crate::state::CgroupFsState>().unwrap();

    let first = state.node(first_group.clone()).unwrap();
    let repeated = state.node(first_group).unwrap();
    assert!(Arc::ptr_eq(&first, &repeated));
    assert!(Arc::ptr_eq(&first.directory(), &repeated.directory()));
    assert!(Arc::ptr_eq(
        &first.file("cgroup.procs").unwrap(),
        &repeated.file("cgroup.procs").unwrap()
    ));

    root.remove_child("child").unwrap();
    let recreated_group = root.create_child("child").unwrap();
    let recreated = state.node(recreated_group).unwrap();
    assert!(!Arc::ptr_eq(&first, &recreated));
    assert!(!Arc::ptr_eq(&first.directory(), &recreated.directory()));
    assert!(!Arc::ptr_eq(
        &first.file("cgroup.procs").unwrap(),
        &recreated.file("cgroup.procs").unwrap()
    ));
}

#[def_test]
fn mount_owner_metadata_survives_other_mount_cache_removal() {
    let root = CgroupNamespace::new().root();
    let group = root.create_child("shared").unwrap();
    let first_sb = crate::new_cgroup2fs(root.clone(), SuperBlockFlags::empty()).unwrap();
    let second_sb = crate::new_cgroup2fs(root, SuperBlockFlags::empty()).unwrap();
    let first_fs = first_sb.private::<Arc<SimpleFs>>().unwrap();
    let second_fs = second_sb.private::<Arc<SimpleFs>>().unwrap();
    let first_state = first_fs.private::<crate::state::CgroupFsState>().unwrap();
    let second_state = second_fs.private::<crate::state::CgroupFsState>().unwrap();
    let mode = kvfs::NodePermission::from_bits_truncate(0o711);
    let first = first_state
        .node_with_owner(group.clone(), mode, 42, 43)
        .unwrap();
    let second = second_state.node(group.clone()).unwrap();
    first_state.remove_node(&group);
    let rematerialized = first_state.node(group).unwrap();
    assert_eq!(rematerialized.directory().metadata().uid, 42);
    assert_eq!(second.directory().metadata().uid, 42);
    assert_eq!(second.directory().metadata().gid, 43);
    assert_ne!(
        first.directory().metadata().inode,
        rematerialized.directory().metadata().inode
    );
}

fn open_test_file(root: &Path, name: &str) -> alloc::sync::Arc<kvfs::VfsFile> {
    Filename::new(name)
        .open_with_flags_at(
            root,
            root,
            0,
            NodePermission::default(),
            NodePermission::default(),
            kcred::initial_cred(),
        )
        .unwrap()
}

const O_RDWR: u32 = 2;

fn open_writable_test_file(root: &Path, name: &str) -> alloc::sync::Arc<kvfs::VfsFile> {
    Filename::new(name)
        .open_with_flags_at(
            root,
            root,
            O_RDWR,
            NodePermission::default(),
            NodePermission::default(),
            kcred::initial_cred(),
        )
        .unwrap()
}

fn read_test_file(root: &Path, name: &str) -> Vec<u8> {
    let file = open_test_file(root, name);
    let mut bytes = [0; 64];
    let count = file.read(&mut bytes).unwrap();
    bytes[..count].to_vec()
}

fn read_pids_events(root: &Path) -> Vec<u8> {
    read_test_file(root, "child/pids.events")
}

#[def_test]
fn pids_events_reports_max_limit_rejections() {
    let root = CgroupNamespace::new().root();
    let super_block = crate::new_cgroup2fs(root.clone(), SuperBlockFlags::empty()).unwrap();
    let root_path = Mount::new_root(&super_block).root_path();
    root.set_pids_subtree_enabled(true).unwrap();
    let child = root.create_child("child").unwrap();
    child.set_pids_max(Some(0)).unwrap();

    let events_before = read_pids_events(&root_path);
    assert_eq!(events_before.as_slice(), b"max 0\n");
    assert!(child.reserve_task().is_err());
    let events_after = read_pids_events(&root_path);
    assert_eq!(events_after.as_slice(), b"max 1\n");
}

#[def_test]
fn controller_files_appear_only_while_the_controller_is_live() {
    let root = CgroupNamespace::new().root();
    let super_block = crate::new_cgroup2fs(root.clone(), SuperBlockFlags::empty()).unwrap();
    let fs = super_block.private::<Arc<SimpleFs>>().unwrap();
    let state = fs.private::<crate::state::CgroupFsState>().unwrap();
    let root_node = state.node(root.clone()).unwrap();

    assert!(root_node.file("cpuset.cpus").is_none());
    assert!(root_node.file("cpuset.cpus.effective").is_some());
    assert!(root_node.file("pids.current").is_none());
    assert!(root_node.file("pids.max").is_none());
    assert!(root_node.file("pids.events").is_none());

    let child = root.create_child("child").unwrap();
    let child_node = state.node(child.clone()).unwrap();
    assert!(child_node.file("cpuset.cpus").is_none());
    assert!(child_node.file("pids.max").is_none());
    assert!(child_node.file("pids.events").is_none());

    root.set_pids_subtree_enabled(true).unwrap();
    assert!(child_node.file("pids.current").is_some());
    assert!(child_node.file("pids.max").is_some());
    assert!(child_node.file("pids.events").is_some());
    assert!(root_node.file("pids.max").is_none());
    assert!(root_node.file("pids.events").is_none());

    root.set_cpuset_subtree_enabled(true).unwrap();
    assert!(child_node.file("cpuset.cpus").is_some());
    assert!(child_node.file("cpuset.cpus.effective").is_some());
    assert!(child_node.file("cpuset.mems").is_some());
    assert!(child_node.file("cpuset.mems.effective").is_some());

    root.set_pids_subtree_enabled(false).unwrap();
    assert!(child_node.file("pids.max").is_none());
    assert!(child_node.file("pids.events").is_none());
    assert!(child_node.file("cpuset.cpus").is_some());
}

#[def_test]
fn memory_control_files_are_visible_only_while_the_controller_is_live() {
    let root = CgroupNamespace::new().root();
    let super_block = crate::new_cgroup2fs(root.clone(), SuperBlockFlags::empty()).unwrap();
    let fs = super_block.private::<Arc<SimpleFs>>().unwrap();
    let state = fs.private::<crate::state::CgroupFsState>().unwrap();
    let root_node = state.node(root.clone()).unwrap();

    assert!(root_node.file("memory.current").is_some());
    assert!(root_node.file("memory.max").is_none());

    let child = root.create_child("memory-child").unwrap();
    let child_node = state.node(child.clone()).unwrap();
    assert!(child_node.file("memory.current").is_none());
    assert!(child_node.file("memory.max").is_none());

    root.set_subtree_control(ControllerKind::Memory, true)
        .unwrap();
    assert!(child_node.file("memory.current").is_some());
    assert!(child_node.file("memory.max").is_some());
    assert!(child_node.file("memory.events.local").is_some());
    assert!(child_node.file("memory.stat").is_some());

    root.set_subtree_control(ControllerKind::Memory, false)
        .unwrap();
    assert!(child_node.file("memory.current").is_none());
    assert!(child_node.file("memory.max").is_none());
}

#[def_test]
fn memory_limits_render_parse_and_round_trip_through_the_vfs() {
    let root = CgroupNamespace::new().root();
    root.set_subtree_control(ControllerKind::Memory, true)
        .unwrap();
    let child = root.create_child("memory-limits").unwrap();
    let super_block = crate::new_cgroup2fs(root.clone(), SuperBlockFlags::empty()).unwrap();
    let root_path = Mount::new_root(&super_block).root_path();

    let max = open_writable_test_file(&root_path, "memory-limits/memory.max");
    assert_eq!(max.write(b"4096\n"), Ok(5));
    assert_eq!(
        read_test_file(&root_path, "memory-limits/memory.max"),
        b"4096\n"
    );
    assert_eq!(max.write(b"max\n"), Ok(4));
    assert_eq!(
        read_test_file(&root_path, "memory-limits/memory.max"),
        b"max\n"
    );
    assert_eq!(max.write(b"garbage\n"), Err(VfsError::InvalidInput));
    assert_eq!(max.write(b"12x\n"), Err(VfsError::InvalidInput));

    let high = open_writable_test_file(&root_path, "memory-limits/memory.high");
    assert_eq!(high.write(b"2048\n"), Ok(5));
    assert_eq!(
        read_test_file(&root_path, "memory-limits/memory.high"),
        b"2048\n"
    );

    let low = open_writable_test_file(&root_path, "memory-limits/memory.low");
    assert_eq!(low.write(b"1024\n"), Ok(5));
    assert_eq!(
        read_test_file(&root_path, "memory-limits/memory.low"),
        b"1024\n"
    );
    assert_eq!(low.write(b"max\n"), Ok(4));
    assert_eq!(
        read_test_file(&root_path, "memory-limits/memory.low"),
        b"max\n"
    );

    let swap_max = open_writable_test_file(&root_path, "memory-limits/memory.swap.max");
    assert_eq!(swap_max.write(b"0\n"), Ok(2));
    assert_eq!(
        read_test_file(&root_path, "memory-limits/memory.swap.max"),
        b"0\n"
    );
    assert_eq!(swap_max.write(b"4096\n"), Err(VfsError::InvalidInput));
    assert_eq!(
        read_test_file(&root_path, "memory-limits/memory.swap.current"),
        b"0\n"
    );

    assert_eq!(
        read_test_file(&root_path, "memory-limits/memory.events.local"),
        b"low 0\nhigh 0\nmax 0\noom 0\noom_kill 0\n"
    );
    assert_eq!(
        read_test_file(&root_path, "memory-limits/memory.stat"),
        b"anon 0\nfile 0\n"
    );
    assert!(child.memory_controller().is_ok());
}

#[def_test]
fn memory_peak_write_zero_resets_only_the_visible_peak() {
    let root = CgroupNamespace::new().root();
    root.set_subtree_control(ControllerKind::Memory, true)
        .unwrap();
    let child = root.create_child("memory-peak").unwrap();
    let super_block = crate::new_cgroup2fs(root.clone(), SuperBlockFlags::empty()).unwrap();
    let root_path = Mount::new_root(&super_block).root_path();
    let peak = open_writable_test_file(&root_path, "memory-peak/memory.peak");

    child
        .memory_controller()
        .unwrap()
        .memcg()
        .set_peak_for_test(8192);
    assert_eq!(peak.write(b"0\n"), Ok(2));
    assert_eq!(
        read_test_file(&root_path, "memory-peak/memory.peak"),
        b"0\n"
    );
    // Linux accepts any numeric write as a reset request; the value itself is
    // not stored as the new peak.
    assert_eq!(peak.write(b"1\n"), Ok(2));
    assert_eq!(child.memory_controller().unwrap().memcg().peak(), 0);
}

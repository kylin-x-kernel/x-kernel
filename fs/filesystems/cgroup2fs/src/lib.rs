// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux cgroup v2 filesystem adapter.
//!
//! The crate translates VFS operations into the canonical cgroup and process
//! ownership APIs. It does not own hierarchy or task-membership state.

#![no_std]
#![deny(unsafe_code)]

extern crate alloc;

mod command;
mod control;
mod controller;
mod dir;
mod mount;
mod pids;
mod process;
mod state;

pub use mount::{FILE_SYSTEM_TYPE, new_cgroup2fs, new_initial_cgroup2fs};

#[cfg(unittest)]
mod tests {
    use alloc::sync::Arc;

    use kcgroup::CgroupNamespace;
    use kvfs::{SimpleFs, SuperBlockFlags, VfsError};
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
        let super_block = crate::new_cgroup2fs(root.clone(), SuperBlockFlags::empty());
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
        let first_sb = crate::new_cgroup2fs(root.clone(), SuperBlockFlags::empty());
        let second_sb = crate::new_cgroup2fs(root, SuperBlockFlags::empty());
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
}

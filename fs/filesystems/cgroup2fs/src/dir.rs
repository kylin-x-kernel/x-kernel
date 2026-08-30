// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{
    borrow::Cow,
    boxed::Box,
    sync::{Arc, Weak},
    vec::Vec,
};

use kcgroup::Cgroup;
use kvfs::{
    Dentry, LockedDentry, SimpleDirLookup, SimpleDirOps, Umode, VfsError, VfsInode, VfsResult,
    inode_init_owner,
};

use crate::state::CgroupFsState;

pub(crate) struct CgroupDir {
    state: Weak<CgroupFsState>,
    cgroup: Arc<Cgroup>,
}

impl CgroupDir {
    pub(crate) fn new(state: Weak<CgroupFsState>, cgroup: Arc<Cgroup>) -> Self {
        Self { state, cgroup }
    }

    fn state(&self) -> VfsResult<Arc<CgroupFsState>> {
        self.state.upgrade().ok_or(VfsError::NoSuchDevice)
    }
}

impl SimpleDirOps for CgroupDir {
    fn child_names<'a>(&'a self) -> VfsResult<Box<dyn Iterator<Item = Cow<'a, str>> + 'a>> {
        let _operation = self.cgroup.begin_operation()?;
        let mut names: Vec<_> = self
            .cgroup
            .child_names()
            .into_iter()
            .map(Cow::Owned)
            .collect();
        if let Ok(state) = self.state()
            && let Some(node) = state.node_if_present(&self.cgroup)
        {
            names.extend(node.file_names().map(Cow::Borrowed));
        }
        Ok(Box::new(names.into_iter()))
    }

    fn lookup_child(&self, lookup: SimpleDirLookup<'_>, name: &str) -> VfsResult<Dentry> {
        let _operation = self.cgroup.begin_operation()?;
        let state = self.state()?;
        if let Some(child) = self.cgroup.child(name) {
            let child = state.node(child)?;
            return Ok(lookup.dir_from_inode(name, child.directory()));
        }
        let node = state
            .node_if_present(&self.cgroup)
            .ok_or(VfsError::NotFound)?;
        let inode = node.file(name).ok_or(VfsError::NotFound)?;
        Ok(lookup.file_from_inode(name, inode))
    }

    fn supports_dentry_cache(&self) -> bool {
        // Controller activation changes which stable file inodes are visible.
        // Retracting lookup dentries prevents a cached negative result from
        // hiding a file after its controller becomes active.
        false
    }

    fn mkdir(
        &self,
        dir: &VfsInode,
        name: &str,
        mode: Umode,
        cred: &kcred::Cred,
    ) -> VfsResult<Arc<VfsInode>> {
        let _operation = self.cgroup.begin_operation()?;
        let state = self.state()?;
        let (mode, uid, gid) = inode_init_owner(dir, mode, cred);
        let child = self.cgroup.create_child(name)?;
        match state.node_with_owner(child, mode.permission(), uid, gid) {
            Ok(node) => Ok(node.directory()),
            Err(error) => {
                let _ = self.cgroup.remove_child(name);
                Err(error)
            }
        }
    }

    fn rmdir(&self, _dir: &VfsInode, victim: &LockedDentry<'_>) -> VfsResult<()> {
        let _operation = self.cgroup.begin_operation()?;
        let state = self.state()?;
        let child = self.cgroup.child(victim.name()).ok_or(VfsError::NotFound)?;
        let node = state.node_if_present(&child).ok_or(VfsError::NotFound)?;
        if !Arc::ptr_eq(&node.directory(), &victim.inode_ref()) {
            return Err(VfsError::NotFound);
        }
        self.cgroup.remove_child_node(&child)?;
        state.remove_node(&child);
        Ok(())
    }
}

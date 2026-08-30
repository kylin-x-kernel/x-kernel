// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
};

use kcgroup::Cgroup;
use klazy::lazy_static;
use ksync::Mutex;
use kvfs::{NodeFlags, NodePermission, SimpleDir, SimpleFs, VfsError, VfsInode, VfsResult};

use crate::{controller, dir::CgroupDir, pids, process};

#[derive(Clone, Copy)]
struct NodeOwner {
    permission: NodePermission,
    uid: u32,
    gid: u32,
    mounts: usize,
}

// Ownership metadata belongs to the hierarchy, rather than a particular
// superblock.  Separate mounts therefore materialize identical permissions.
lazy_static! {
    static ref HIERARCHY_OWNERS: Mutex<BTreeMap<usize, BTreeMap<usize, NodeOwner>>> =
        Mutex::new(BTreeMap::new());
}

pub(crate) struct CgroupFsState {
    fs: Weak<SimpleFs>,
    view_root: Arc<Cgroup>,
    nodes: Mutex<BTreeMap<usize, Arc<CgroupFsNode>>>,
}

pub(crate) struct CgroupFsNode {
    cgroup: Arc<Cgroup>,
    directory: Arc<VfsInode>,
    files: BTreeMap<&'static str, Arc<VfsInode>>,
}

impl CgroupFsState {
    pub(crate) fn new(fs: &Arc<SimpleFs>, view_root: Arc<Cgroup>) -> Arc<Self> {
        Arc::new(Self {
            fs: Arc::downgrade(fs),
            view_root,
            nodes: Mutex::new(BTreeMap::new()),
        })
    }

    pub(crate) fn node(self: &Arc<Self>, cgroup: Arc<Cgroup>) -> VfsResult<Arc<CgroupFsNode>> {
        self.node_with_owner(cgroup, NodePermission::from_bits_truncate(0o755), 0, 0)
    }

    pub(crate) fn node_with_owner(
        self: &Arc<Self>,
        cgroup: Arc<Cgroup>,
        permission: NodePermission,
        uid: u32,
        gid: u32,
    ) -> VfsResult<Arc<CgroupFsNode>> {
        let key = Arc::as_ptr(&cgroup).addr();
        let hierarchy_key = Arc::as_ptr(&cgroup.hierarchy_root()).addr();
        let mut nodes = self.nodes.lock();
        if let Some(node) = nodes.get(&key) {
            return Ok(node.clone());
        }
        let owner = {
            let mut owners = HIERARCHY_OWNERS.lock();
            let hierarchy = owners.entry(hierarchy_key).or_default();
            let owner = hierarchy.entry(key).or_insert(NodeOwner {
                permission,
                uid,
                gid,
                mounts: 0,
            });
            owner.mounts = owner.mounts.saturating_add(1);
            *owner
        };
        let fs = self.fs.upgrade().ok_or(VfsError::NoSuchDevice)?;
        let directory = SimpleDir::new_inode_with_owner(
            fs.clone(),
            Arc::new(CgroupDir::new(Arc::downgrade(self), cgroup.clone())),
            owner.permission,
            owner.uid,
            owner.gid,
        );
        let mut files = BTreeMap::new();
        let declarations = [
            (
                "cgroup.controllers",
                controller::controllers_file(fs.clone(), cgroup.clone()),
            ),
            (
                "cgroup.subtree_control",
                controller::subtree_control_file(fs.clone(), cgroup.clone()),
            ),
            (
                "cgroup.procs",
                process::membership_file(fs.clone(), cgroup.clone(), Arc::downgrade(self)),
            ),
            (
                "pids.current",
                pids::current_file(fs.clone(), cgroup.clone()),
            ),
            ("pids.max", pids::max_file(fs, cgroup.clone())),
        ];
        for (name, file) in declarations {
            files.insert(name, file.new_inode(NodeFlags::empty()));
        }
        let node = Arc::new(CgroupFsNode {
            cgroup,
            directory,
            files,
        });
        nodes.insert(key, node.clone());
        Ok(node)
    }

    pub(crate) fn view_root(&self) -> &Arc<Cgroup> {
        &self.view_root
    }

    pub(crate) fn node_if_present(&self, cgroup: &Arc<Cgroup>) -> Option<Arc<CgroupFsNode>> {
        self.nodes.lock().get(&Arc::as_ptr(cgroup).addr()).cloned()
    }

    pub(crate) fn remove_node(&self, cgroup: &Arc<Cgroup>) {
        self.nodes.lock().remove(&Arc::as_ptr(cgroup).addr());
        let hierarchy_key = Arc::as_ptr(&cgroup.hierarchy_root()).addr();
        let key = Arc::as_ptr(cgroup).addr();
        let mut owners = HIERARCHY_OWNERS.lock();
        if let Some(hierarchy) = owners.get_mut(&hierarchy_key) {
            if let Some(owner) = hierarchy.get_mut(&key) {
                owner.mounts = owner.mounts.saturating_sub(1);
                if owner.mounts == 0 {
                    hierarchy.remove(&key);
                }
            }
            if hierarchy.is_empty() {
                owners.remove(&hierarchy_key);
            }
        }
    }
}

impl CgroupFsNode {
    pub(crate) fn directory(&self) -> Arc<VfsInode> {
        self.directory.clone()
    }

    pub(crate) fn file(&self, name: &str) -> Option<Arc<VfsInode>> {
        if matches!(name, "pids.current" | "pids.max") && !self.cgroup.has_pids_controller() {
            return None;
        }
        self.files.get(name).cloned()
    }

    pub(crate) fn file_names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.files.keys().copied().filter(|name| {
            !matches!(*name, "pids.current" | "pids.max") || self.cgroup.has_pids_controller()
        })
    }
}

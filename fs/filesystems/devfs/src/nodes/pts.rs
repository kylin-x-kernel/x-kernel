// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! PTY node projection; pair admission and number ownership belong to ktty.

use alloc::{
    borrow::Cow,
    string::ToString,
    sync::{Arc, Weak},
};

use inherit_methods_macro::inherit_methods;
use kalloc_types::KBox;
use kerrno::{KError, KResult};
use ktty::tty::{self, PtyPair};
use kvfs::{
    Dentry, DeviceFileOps, FileOperations, InodeOperations, Metadata, MetadataUpdate, NodeFlags,
    NodePermission, NodeType, SimpleDirLookup, SimpleDirOps, SimpleFs, SimpleFsNode,
    VfsFileBuilder, VfsInode, VfsResult,
};

use crate::DeviceFile;

struct SlaveNode {
    node: SimpleFsNode,
    pair: Weak<PtyPair>,
}

impl SlaveNode {
    fn inode(fs: Arc<SimpleFs>, pair: &Arc<PtyPair>) -> Arc<VfsInode> {
        let node = SimpleFsNode::new(fs, NodeType::CharacterDevice, NodePermission::default());
        node.set_rdev(pair.slave_device_id());
        let init = node.inode_init();
        let node = Arc::new(Self {
            node,
            pair: Arc::downgrade(pair),
        });
        VfsInode::new_file_with_operations(
            node.clone(),
            node.clone(),
            node,
            NodeFlags::NON_CACHEABLE,
            init,
        )
    }
}

#[inherit_methods(from = "self.node")]
impl InodeOperations for SlaveNode {
    fn getattr(
        &self,
        idmap: &kvfs::MountIdmap,
        path: Option<&kvfs::Path>,
        request_mask: kvfs::GetattrRequestMask,
        query_flags: kvfs::GetattrQueryFlags,
    ) -> VfsResult<Metadata>;

    fn setattr(
        &self,
        idmap: &kvfs::MountIdmap,
        dentry: &kvfs::Dentry,
        update: MetadataUpdate,
    ) -> VfsResult<MetadataUpdate>;
}

impl FileOperations for SlaveNode {
    fn open(self: Arc<Self>, inode: &VfsInode, file: &mut VfsFileBuilder) -> VfsResult<()> {
        self.pair
            .upgrade()
            .ok_or(KError::Io)?
            .open_slave(inode, file)
    }
}

/// Master pseudo-terminal multiplexer at `pts/ptmx` in devtmpfs.
///
/// devtmpfs exposes `/dev/ptmx` as a symlink to this path, the conventional
/// userspace layout. When devpts is mounted at `/dev/pts`, that target
/// resolves to the devpts instance's own `ptmx`; before such a mount, it
/// resolves here.
///
/// The carried filesystem is this mount's owner: pairs allocated through this
/// fallback are published here and projected by [`PtsDir`], and never leak
/// into (or from) a container's per-mount devpts instance. Linux selects the
/// target instance from the ptmx file's path (`ptmx_open()` →
/// `devpts_acquire()`), so the symlink gives the same path-based choice; see
/// the devpts design notes.
pub struct Ptmx(pub Arc<SimpleFs>);

impl DeviceFileOps for Ptmx {
    fn open(&self, inode: &VfsInode, file: &mut VfsFileBuilder) -> VfsResult<()> {
        tty::open_ptmx(&self.0, inode, file, |pair| {
            Ok(SlaveNode::inode(self.0.clone(), pair))
        })
    }
}

/// devtmpfs `/dev/pts` directory: its own fallback `ptmx` plus slaves with a
/// live master **allocated through that fallback** — per-owner projection
/// keeps a container's per-mount devpts pairs out of the host view.
pub(crate) struct PtsDir {
    fs: Arc<SimpleFs>,
    ptmx: Option<Arc<DeviceFile>>,
}

impl PtsDir {
    pub(crate) fn new(fs: Arc<SimpleFs>, ptmx: Option<Arc<DeviceFile>>) -> Self {
        Self { fs, ptmx }
    }
}

impl SimpleDirOps for PtsDir {
    fn supports_dentry_cache(&self) -> bool {
        // Both missing and present names change as pairs are created/closed.
        // Retained dentries stay bound to the old pair, never its successor.
        false
    }

    fn child_names<'a>(&'a self) -> VfsResult<KBox<dyn Iterator<Item = Cow<'a, str>> + 'a>> {
        let ptmx = self.ptmx.iter().map(|_| Cow::Borrowed("ptmx"));
        Ok(KBox::new(
            ptmx.chain(
                tty::pty_slave_numbers_for(&self.fs)
                    .into_iter()
                    .map(|id| Cow::Owned(id.to_string())),
            ),
        )?)
    }

    fn lookup_child(&self, lookup: SimpleDirLookup<'_>, name: &str) -> KResult<Dentry> {
        if name == "ptmx" {
            let ptmx = self.ptmx.as_ref().ok_or(KError::NotFound)?;
            return Ok(crate::device_dentry(lookup, name, ptmx.clone()));
        }
        // Only the canonical decimal form (what readdir lists, Linux dcache
        // exact-match semantics): "007" must not alias "7". The devpts
        // directory enforces the same rule on the same names.
        let number = name
            .parse::<u32>()
            .ok()
            .filter(|number| number.to_string() == name)
            .ok_or(KError::NotFound)?;
        let inode = tty::pty_slave_inode_for(&self.fs, number).ok_or(KError::NotFound)?;
        Ok(lookup.file_from_inode(name, inode))
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/pts.rs");

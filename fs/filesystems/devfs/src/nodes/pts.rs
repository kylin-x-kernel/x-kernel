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

/// Master pseudo-terminal multiplexer (/dev/ptmx).
pub struct Ptmx(pub Arc<SimpleFs>);

impl DeviceFileOps for Ptmx {
    fn open(&self, inode: &VfsInode, file: &mut VfsFileBuilder) -> VfsResult<()> {
        tty::open_ptmx(inode, file, |pair| {
            Ok(SlaveNode::inode(self.0.clone(), pair))
        })
    }
}

/// /dev/pts directory containing only slaves with a live master.
pub(crate) struct PtsDir;

impl SimpleDirOps for PtsDir {
    fn supports_dentry_cache(&self) -> bool {
        // Both missing and present names change as pairs are created/closed.
        // Retained dentries stay bound to the old pair, never its successor.
        false
    }

    fn child_names<'a>(&'a self) -> VfsResult<KBox<dyn Iterator<Item = Cow<'a, str>> + 'a>> {
        Ok(KBox::new(
            tty::pty_slave_numbers()
                .into_iter()
                .map(|id| Cow::Owned(id.to_string())),
        )?)
    }

    fn lookup_child(&self, lookup: SimpleDirLookup<'_>, name: &str) -> KResult<Dentry> {
        let number = name.parse::<u32>().map_err(|_| KError::NotFound)?;
        let inode = tty::pty_slave_inode(number).ok_or(KError::NotFound)?;
        Ok(lookup.file_from_inode(name, inode))
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/pts.rs");

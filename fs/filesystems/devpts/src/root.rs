// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Root directory of a devpts mount: `ptmx` plus this instance's live slaves.

use alloc::{
    borrow::Cow,
    string::ToString,
    sync::{Arc, Weak},
    vec::Vec,
};

use inherit_methods_macro::inherit_methods;
use kalloc_types::KBox;
use kerrno::{KError, KResult};
use ktty::tty::{self, PtyPair};
use kvfs::{
    DeviceId, DirMaker, FileOperations, InodeOperations, Metadata, MetadataUpdate, NodeFlags,
    NodePermission, NodeType, SimpleDir, SimpleDirLookup, SimpleDirOps, SimpleFs, SimpleFsNode,
    VfsFileBuilder, VfsInode, VfsInodeInit, VfsResult,
};

use crate::MountOptions;

/// `TTY_AUX_MAJOR`/`PTMX_MINOR`: the Unix98 ptmx device (Linux `devpts`).
const PTMX_DEVICE_ID: DeviceId = DeviceId::new(5, 2);

/// Name of the master multiplexer node mandated by the Unix98 pty layout.
pub(super) const PTMX_NAME: &str = "ptmx";

/// Builds the root directory of one devpts mount.
pub(super) fn builder(fs: Arc<SimpleFs>, options: MountOptions, cred: &kcred::Cred) -> DirMaker {
    // The mount's options are shared with `devpts_reconfigure()`: a remount
    // replaces them for slaves created afterwards, and updates the live ptmx
    // inode's mode immediately (Linux `devpts_reconfigure()`).
    let options = Arc::new(ksync::Mutex::new(options));
    fs.set_private(options.clone());
    let ptmx = DevptsPtmx::new(fs.clone(), options, cred);
    let ops = Arc::new(RootOps {
        fs: fs.clone(),
        ptmx: ptmx.inode(),
    });
    // Linux's devpts root is `S_IFDIR | S_IRUGO | S_IXUGO | S_IWUSR` (0755),
    // which is also this constructor's default; `mode=`/`gid=` apply to the
    // pty nodes, not the directory.
    SimpleDir::new_maker(fs, ops)
}

/// Resolves the (uid, gid) owner for a new slave pty node the way Linux
/// `devpts_pty_new` does: the explicit `uid=`/`gid=` mount option when set,
/// otherwise the acting task's filesystem ids.
///
/// Runs in user-task context: without `uid=`/`gid=`, the owner comes from
/// `kprocess::current_cred()`, which panics for a kernel task. Production
/// callers are `open(2)` paths (`DevptsPtmx::open`); the mount-time
/// constructor deliberately does not go through here (see `DevptsPtmx::new`).
fn slave_ownership(options: MountOptions) -> (u32, u32) {
    // The task credentials are consulted only when the corresponding mount
    // option is absent, so a fully explicit `uid=`/`gid=` mount works from a
    // kernel task too.
    let uid = match options.set_uid {
        true => options.uid,
        false => kprocess::current_cred().fsuid(),
    };
    let gid = match options.set_gid {
        true => options.gid,
        false => kprocess::current_cred().fsgid(),
    };
    (uid, gid)
}

struct RootOps {
    fs: Arc<SimpleFs>,
    ptmx: Arc<VfsInode>,
}

impl SimpleDirOps for RootOps {
    fn child_names<'a>(&'a self) -> VfsResult<KBox<dyn Iterator<Item = Cow<'a, str>> + 'a>> {
        let mut names: Vec<Cow<'a, str>> = Vec::from([Cow::Borrowed(PTMX_NAME)]);
        names.extend(
            tty::pty_slave_numbers_for(&self.fs)
                .into_iter()
                .map(|number| Cow::Owned(number.to_string())),
        );
        Ok(KBox::new(names.into_iter())?)
    }

    fn lookup_child(&self, lookup: SimpleDirLookup<'_>, name: &str) -> KResult<kvfs::Dentry> {
        if name == PTMX_NAME {
            return Ok(lookup.file_from_inode(name, self.ptmx.clone()));
        }
        // Only the canonical decimal form (what readdir lists, Linux
        // dcache exact-match semantics): "007" must not alias "7".
        let number = name
            .parse::<u32>()
            .ok()
            .filter(|number| number.to_string() == name)
            .ok_or(KError::NotFound)?;
        // Owner-scoped resolution: the mount's filesystem is the pty owner, so
        // a number recycled by another instance after this mount's pair died
        // can never resolve to that instance's slave inode here.
        let inode = tty::pty_slave_inode_for(&self.fs, number).ok_or(KError::NotFound)?;
        Ok(lookup.file_from_inode(name, inode))
    }

    fn supports_dentry_cache(&self) -> bool {
        // Both missing and present names change as pairs are created/closed.
        // Retained dentries stay bound to the old pair, never its successor.
        false
    }
}

/// The `ptmx` master multiplexer node of one devpts instance.
///
/// Opening it allocates the pty pair through the shared devfs PTY driver and
/// publishes the slave inode inside THIS filesystem, which is what makes the
/// pair's `/dev/pts/<n>` resolvable in this mount.
struct DevptsPtmx {
    node: SimpleFsNode,
    fs: Arc<SimpleFs>,
    /// This mount's current options; shared with `devpts_reconfigure()`.
    options: Arc<ksync::Mutex<MountOptions>>,
}

impl DevptsPtmx {
    fn new(
        fs: Arc<SimpleFs>,
        options: Arc<ksync::Mutex<MountOptions>>,
        cred: &kcred::Cred,
    ) -> Arc<Self> {
        // Linux `mknod_ptmx()` (called from `devpts_fill_super`) pins the
        // `ptmx` inode to `current_fsuid()`/`current_fsgid()` and never
        // consults `uid=`/`gid=` — those options belong to the slave nodes
        // alone. This constructor deliberately uses the mount context's
        // credentials instead of the current task's: mount(2) is issued by
        // the mounting task, so the two agree on the mount(2) path
        // (`fc->cred = get_current_cred()`), and construction no longer
        // depends on the caller being a user task.

        let node = SimpleFsNode::new_with_owner(
            fs.clone(),
            NodeType::CharacterDevice,
            NodePermission::from_bits_truncate(options.lock().ptmx_mode),
            cred.fsuid(),
            cred.fsgid(),
        );
        node.set_rdev(PTMX_DEVICE_ID);
        Arc::new(Self { node, fs, options })
    }

    fn inode(self: &Arc<Self>) -> Arc<VfsInode> {
        let init: VfsInodeInit = self.node.inode_init();
        VfsInode::new_file_with_operations(
            self.clone(),
            self.clone(),
            self.clone(),
            NodeFlags::NON_CACHEABLE,
            init,
        )
    }
}

#[inherit_methods(from = "self.node")]
impl InodeOperations for DevptsPtmx {
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

impl FileOperations for DevptsPtmx {
    fn open(self: Arc<Self>, inode: &VfsInode, file: &mut VfsFileBuilder) -> VfsResult<()> {
        let owner = self.fs.clone();
        let slave_fs = self.fs.clone();
        let options = *self.options.lock();
        tty::open_ptmx(&owner, inode, file, move |pair| {
            Ok(DevptsSlave::inode(slave_fs.clone(), pair, options))
        })
    }
}

/// One instance's slave pty node, bound to its pair by a weak reference.
struct DevptsSlave {
    node: SimpleFsNode,
    pair: Weak<PtyPair>,
}

impl DevptsSlave {
    fn inode(fs: Arc<SimpleFs>, pair: &Arc<PtyPair>, options: MountOptions) -> Arc<VfsInode> {
        // Linux assigns the slave the uid/gid of the task allocating the pty
        // (the ptmx opener) unless `uid=`/`gid=` overrode them.
        let (uid, gid) = slave_ownership(options);
        let node = SimpleFsNode::new_with_owner(
            fs,
            NodeType::CharacterDevice,
            NodePermission::from_bits_truncate(options.slave_mode),
            uid,
            gid,
        );
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
impl InodeOperations for DevptsSlave {
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

impl FileOperations for DevptsSlave {
    fn open(self: Arc<Self>, inode: &VfsInode, file: &mut VfsFileBuilder) -> VfsResult<()> {
        self.pair
            .upgrade()
            .ok_or(KError::Io)?
            .open_slave(inode, file)
    }
}

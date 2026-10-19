// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Mountpoints and location resolution for the VFS.
use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
    vec,
    vec::Vec,
};
use core::{
    iter,
    sync::atomic::{AtomicU32, AtomicU64, Ordering},
};

use hashbrown::HashMap;
use kcred::{Cred, NamespaceId, UserNamespace, initial_user_namespace};
use klazy::Once;
use ktime_types::TimeSpan;

/// Mount idmapping context passed into inode namespace operations.
#[derive(Debug)]
pub struct MountIdmap;

use crate::{
    Dentry, DentryKey, DeviceId, FMode, FileOperations, FsContext, GetattrQueryFlags,
    GetattrRequestMask, InodeUpdateTime, Metadata, MetadataUpdate, Mutex, NodeFlags,
    NodePermission, NodeType, OpenFlags, Permission, RenameFlags, SetattrTime, StatFs, SuperBlock,
    SuperBlockFlags, Umode, VfsError, VfsFile, VfsFileBuilder, VfsInode, VfsResult, nullfs,
    path::PathBuf,
};

bitflags::bitflags! {
    /// Per-mount flags, converted from the user-visible `MS_*` constants
    /// during the `mount(2)` syscall.
    ///
    /// See [`per_mount_flags`] in `posix/fs/src/mount.rs` for the mapping.
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct MountFlags: u32 {
        /// Mount read-only.
        const RDONLY = 0x40;
        /// Ignore set-user-ID and set-group-ID bits on executables.
        const NOSUID = 0x01;
        /// Disallow access to device special files on this mount.
        const NODEV = 0x02;
        /// Disallow program execution from this mount.
        const NOEXEC = 0x04;
        /// Do not update file access times.
        const NOATIME = 0x08;
        /// Do not update directory access times.
        const NODIRATIME = 0x10;
        /// Update access time relative to mtime/ctime (the default).
        const RELATIME = 0x20;
        /// Do not follow symlinks on this mount.
        const NOSYMFOLLOW = 0x80;
    }
}

/// `struct vfsmount`.
///
/// Releasing the final object may synchronously run final superblock shutdown
/// and sleep. It must therefore happen in sleepable task context, without
/// holding a non-sleepable lock or a lock required by filesystem shutdown.
#[derive(Debug)]
pub(crate) struct VfsMount {
    mnt_root: Dentry,
    mnt_sb: Arc<SuperBlock>,
    mnt_flags: AtomicU32,
}

impl VfsMount {
    fn try_new(
        fs: &Arc<SuperBlock>,
        flags: MountFlags,
        requested_superblock_flags: Option<SuperBlockFlags>,
    ) -> VfsResult<Option<Self>> {
        Ok(fs
            .try_activate_mount(requested_superblock_flags)?
            .then(|| Self {
                mnt_root: fs.root_dir(),
                mnt_sb: fs.clone(),
                mnt_flags: AtomicU32::new(flags.bits()),
            }))
    }

    fn clone_from_path(source: &Path) -> Self {
        let super_block = source.super_block().clone();
        super_block.activate_mount();
        Self {
            mnt_root: source.dentry.clone(),
            mnt_sb: super_block,
            mnt_flags: AtomicU32::new(source.mount().flags().bits()),
        }
    }

    // Mount flags neither publish nor guard other state, so relaxed ordering
    // is sufficient for both snapshots and replacements.
    fn flags(&self) -> MountFlags {
        MountFlags::from_bits_truncate(self.mnt_flags.load(Ordering::Relaxed))
    }

    fn set_flags(&self, flags: MountFlags) {
        self.mnt_flags.store(flags.bits(), Ordering::Relaxed);
    }
}

impl Drop for VfsMount {
    fn drop(&mut self) {
        self.mnt_sb.deactivate_mount();
    }
}

static INIT_MNT_NS: Once<Arc<MntNamespace>> = Once::new();
static MOUNT_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

fn new_initial_nullfs_superblock() -> Arc<SuperBlock> {
    nullfs::new_superblock()
}

/// `struct mnt_namespace`.
#[derive(Debug)]
pub struct MntNamespace {
    id: NamespaceId,
    user_ns: Arc<UserNamespace>,
    root: Arc<Mount>,
    mounts: Mutex<HashMap<u64, Arc<Mount>>>,
}

impl MntNamespace {
    /// Initializes the global initial mount namespace mount tree.
    pub fn init_mount_tree(root_fs: &Arc<SuperBlock>) -> Arc<Self> {
        Arc::clone(INIT_MNT_NS.call_once(|| {
            Self::build_initial_mount_tree(root_fs)
                .expect("initial mount namespace tree must be created")
        }))
    }

    /// Returns the global initial mount namespace.
    pub fn initial() -> VfsResult<Arc<Self>> {
        INIT_MNT_NS.get().cloned().ok_or(VfsError::InvalidInput)
    }

    /// Creates a mount namespace rooted at the given filesystem.
    pub fn new_root(root_fs: &Arc<SuperBlock>, user_ns: Arc<UserNamespace>) -> Arc<Self> {
        let root = Mount::new_root(root_fs);
        let namespace = Arc::new(Self {
            id: NamespaceId::new(),
            user_ns,
            root: root.clone(),
            mounts: Mutex::default(),
        });
        namespace.mounts.lock().insert(root.mount_id(), root);
        namespace
    }

    fn build_initial_mount_tree(root_fs: &Arc<SuperBlock>) -> VfsResult<Arc<Self>> {
        let namespace_root = new_initial_nullfs_superblock();
        let namespace = Self::new_root(&namespace_root, initial_user_namespace());
        namespace.attach_with_flags_and_devname(
            &namespace.root_path(),
            root_fs,
            MountFlags::RELATIME,
            None,
        )?;
        Ok(namespace)
    }

    /// Creates a private copy of this mount namespace and retargets paths into it.
    ///
    /// The cloned namespace receives new [`Mount`] objects that point at the same
    /// mounted filesystem roots as the source tree. Dentries and superblocks are
    /// shared with the source filesystem instances, matching Linux's mount-tree
    /// copy semantics.
    pub fn clone_with_root_and_pwd(&self, root: &Path, pwd: &Path) -> VfsResult<NamespaceClone> {
        let _mounts_guard = self.mounts.lock();
        let mut mount_map = HashMap::new();
        let cloned_root = clone_mount_tree(&self.root, None, &mut mount_map);
        let namespace = Arc::new(Self {
            id: NamespaceId::new(),
            user_ns: self.user_ns.clone(),
            root: cloned_root,
            mounts: Mutex::default(),
        });
        {
            let mut mounts = namespace.mounts.lock();
            for mount in mount_map.values() {
                mounts.insert(mount.mount_id(), mount.clone());
            }
        }

        Ok(NamespaceClone {
            namespace,
            root: retarget_path(root, &mount_map)?,
            pwd: retarget_path(pwd, &mount_map)?,
        })
    }

    /// Returns this namespace's ID.
    pub fn id(&self) -> NamespaceId {
        self.id
    }

    /// Returns the user namespace that owns this mount namespace.
    pub fn user_ns(&self) -> &Arc<UserNamespace> {
        &self.user_ns
    }

    /// Returns this namespace's root mount.
    pub fn root_mount(&self) -> &Arc<Mount> {
        &self.root
    }

    /// Returns this namespace's root path.
    pub fn root_path(&self) -> Path {
        self.root.root_path()
    }

    /// Returns the path seen as `/` by tasks attached to this namespace.
    ///
    /// The initial namespace keeps a non-user-visible structural root mount and
    /// layers the mutable root filesystem over it, matching Linux
    /// `init_mount_tree()`. Process `fs_struct` root and pwd should use this
    /// visible path, not the namespace's structural root path.
    pub fn visible_root_path(&self) -> Path {
        self.root_path().resolve_final_mount()
    }

    /// Mounts a filesystem at a resolved mountpoint without per-mount flags.
    pub fn attach(&self, mountpoint: &Path, fs: &Arc<SuperBlock>) -> VfsResult<Arc<Mount>> {
        self.attach_with_flags_and_devname(mountpoint, fs, MountFlags::empty(), None)
    }

    /// Mounts a filesystem with per-mount flags and device name at a resolved mountpoint.
    pub fn attach_with_flags_and_devname(
        &self,
        mountpoint: &Path,
        fs: &Arc<SuperBlock>,
        flags: MountFlags,
        devname: Option<&str>,
    ) -> VfsResult<Arc<Mount>> {
        self.attach_with_requested_superblock_flags(mountpoint, fs, flags, devname, None)
    }

    fn attach_with_requested_superblock_flags(
        &self,
        mountpoint: &Path,
        fs: &Arc<SuperBlock>,
        flags: MountFlags,
        devname: Option<&str>,
        requested_superblock_flags: Option<SuperBlockFlags>,
    ) -> VfsResult<Arc<Mount>> {
        let mut mounts = self.mounts.lock();
        Self::require_mount(&mounts, mountpoint.mount())?;
        let mount = mountpoint.mount_filesystem_with_requested_superblock_flags(
            fs,
            flags,
            devname,
            requested_superblock_flags,
        )?;
        mounts.insert(mount.mount_id(), mount.clone());
        Ok(mount)
    }

    /// Creates and attaches a filesystem selected by a filesystem context.
    ///
    /// Filesystem construction completes before the namespace lock is
    /// acquired, so `get_tree` may perform blocking device I/O. If the
    /// selected superblock enters final shutdown before activation, lookup is
    /// repeated just as Linux retries after losing the `sget` race.
    pub fn mount_new(
        &self,
        mountpoint: &Path,
        mount_flags: MountFlags,
        context: &mut FsContext<'_>,
        lookup_root: &Path,
        lookup_pwd: &Path,
    ) -> VfsResult<Arc<Mount>> {
        loop {
            let super_block = context.get_tree(lookup_root, lookup_pwd)?;
            let result = self.attach_with_requested_superblock_flags(
                mountpoint,
                &super_block,
                mount_flags,
                context.source(),
                Some(context.sb_flags()),
            );
            if matches!(&result, Err(VfsError::ResourceBusy)) && !super_block.is_available() {
                continue;
            }
            return result;
        }
    }

    /// Creates a non-recursive bind mount of `source` at `mountpoint`.
    ///
    /// The new mount shares the source dentry and superblock and inherits the
    /// source mount's per-mount flags.
    pub fn attach_bind(&self, source: &Path, mountpoint: &Path) -> VfsResult<Arc<Mount>> {
        let mut mounts = self.mounts.lock();
        Self::require_mount(&mounts, source.mount())?;
        Self::require_mount(&mounts, mountpoint.mount())?;
        let mount = mountpoint.mount_bind(source)?;
        mounts.insert(mount.mount_id(), mount.clone());
        Ok(mount)
    }

    /// Remounts `target` with independent superblock and per-mount flags.
    ///
    /// Returns [`VfsError::InvalidInput`] unless `target` is the root path of
    /// a mount registered in this namespace.
    pub fn remount(
        &self,
        target: &Path,
        context: &mut FsContext<'_>,
        mount_flags: MountFlags,
    ) -> VfsResult<()> {
        let mounts = self.mounts.lock();
        let mount = Self::registered_mount_at(&mounts, target)?;
        mount.super_block().reconfigure(context)?;
        mount.set_flags(mount_flags);
        Ok(())
    }

    /// Replaces only the per-mount flags of the mount rooted at `target`.
    pub fn reconfigure_mount(&self, target: &Path, flags: MountFlags) -> VfsResult<()> {
        let mounts = self.mounts.lock();
        let mount = Self::registered_mount_at(&mounts, target)?;
        mount.set_flags(flags);
        Ok(())
    }

    /// Makes the mount rooted at `target` private.
    ///
    /// KVFS does not yet model shared propagation groups, so registered mounts
    /// are private by construction and this operation is idempotent.
    ///
    /// # Errors
    ///
    /// Returns [`VfsError::InvalidInput`] unless `target` is the root of a
    /// mount registered in this namespace.
    pub fn make_mount_private(&self, target: &Path) -> VfsResult<()> {
        let mounts = self.mounts.lock();
        Self::registered_mount_at(&mounts, target)?;
        Ok(())
    }

    /// Makes the mount rooted at `target` a slave.
    ///
    /// KVFS does not yet model shared propagation groups, so no mount can ever
    /// have a shared master: a slave mount would receive no mount events and is
    /// therefore behaviorally identical to a private mount. This operation only
    /// validates the target and is idempotent.
    ///
    /// # Errors
    ///
    /// Returns [`VfsError::InvalidInput`] unless `target` is the root of a
    /// mount registered in this namespace.
    pub fn make_mount_slave(&self, target: &Path) -> VfsResult<()> {
        let mounts = self.mounts.lock();
        Self::registered_mount_at(&mounts, target)?;
        Ok(())
    }

    fn registered_mount_at<'a>(
        mounts: &HashMap<u64, Arc<Mount>>,
        target: &'a Path,
    ) -> VfsResult<&'a Arc<Mount>> {
        if !target.is_mount_root() {
            return Err(VfsError::InvalidInput);
        }

        let mount = target.mount();
        Self::require_mount(mounts, mount)?;
        Ok(mount)
    }

    /// Linux `is_path_reachable(path_mnt, path.dentry, from)`: whether `path`
    /// lies at or below `from`, walking across mount boundaries. Equality is
    /// reachable, matching `is_subdir(dentry, dentry)`.
    fn is_reachable_from(path: &Path, from: &Path) -> bool {
        let mut current = path.clone();
        loop {
            if current.ptr_eq(from) {
                return true;
            }
            match current.parent() {
                Some(parent) => current = parent,
                None => return false,
            }
        }
    }

    fn require_mount(mounts: &HashMap<u64, Arc<Mount>>, mount: &Arc<Mount>) -> VfsResult<()> {
        mounts
            .get(&mount.mount_id())
            .is_some_and(|registered| Arc::ptr_eq(registered, mount))
            .then_some(())
            .ok_or(VfsError::InvalidInput)
    }

    /// Returns whether a mount other than `path`'s own is attached at or below
    /// `path` in this namespace.
    ///
    /// Recursive bind consults this to decide whether cloning a single mount
    /// would flatten a source subtree. It walks the namespace's registered
    /// mounts and tests each mountpoint against `path`, rather than only the
    /// source mount's direct children.
    pub fn has_mount_under(&self, path: &Path) -> bool {
        let source = path.mount();
        let mounts = self.mounts.lock();
        mounts.values().any(|mount| {
            !Arc::ptr_eq(mount, source)
                && mount
                    .location()
                    .is_some_and(|loc| Self::is_reachable_from(&loc, path))
        })
    }

    /// Unmounts one visible mount from this namespace.
    pub fn detach(&self, path: &Path) -> VfsResult<()> {
        let mut mounts = self.mounts.lock();
        let removed = Self::registered_mount_at(&mounts, path)?.clone();
        path.unmount()?;
        mounts.remove(&removed.mount_id());
        drop(mounts);
        Ok(())
    }

    /// Unmounts a visible mount tree from this namespace.
    pub fn detach_tree(&self, path: &Path) -> VfsResult<()> {
        let mut mounts = self.mounts.lock();
        let root = Self::registered_mount_at(&mounts, path)?.clone();
        let removed = root.collect_subtree();
        for mount in &removed {
            Self::require_mount(&mounts, mount)?;
        }

        path.detach_mount_subtree(&removed)?;
        for mount in &removed {
            mounts.remove(&mount.mount_id());
        }
        drop(mounts);
        Ok(())
    }

    /// Pivots the namespace root to `new_root` and relocates the previous root
    /// mount at `put_old` (Linux `pivot_root(2)`).
    ///
    /// Requirements mirror Linux `do_pivot_root()`:
    /// - `new_root` must be the root of a mount registered in this namespace
    ///   and must not be the current visible root mount (`new_mnt == root_mnt`
    ///   is a loop, `EBUSY`);
    /// - `put_old` must be a directory reachable from `new_root`
    ///   (`is_path_reachable(old_mnt, old.dentry, &new)`) and must not live in
    ///   the current root's filesystem (`old_mnt == root_mnt`, `EBUSY`).
    ///   `put_old` may coincide with `new_root` itself — OCI runtimes pivot
    ///   with `pivot_root(".", ".")`, leaving the old root covering the new
    ///   root until they `umount2(MNT_DETACH)` it;
    /// - both paths must resolve inside this namespace.
    ///
    /// After the call, `/` resolves to `new_root`'s filesystem and the previous
    /// root mount is attached at `put_old` (OCI runtimes then detach it with
    /// `umount2(MNT_DETACH)`). The caller is expected to retarget its
    /// `FsStruct` root/pwd afterwards (the syscall adapter does this), matching
    /// Linux `chroot_fs_refs`.
    ///
    /// # Errors
    ///
    /// Returns [`VfsError::ResourceBusy`] when `new_root` or `put_old` lies on
    /// the current root filesystem (Linux `EBUSY`, "loop, on the same file
    /// system"), [`VfsError::InvalidInput`] for other namespace-degenerate
    /// pivots (no stacked visible root, `new_root` equal to the visible root,
    /// `put_old` unreachable from `new_root`), and [`VfsError::NotADirectory`]
    /// when `put_old` is not a directory.
    pub fn pivot_root(
        &self,
        new_root: &Path,
        put_old: &Path,
        current_root: &Path,
    ) -> VfsResult<()> {
        let mounts = self.mounts.lock();
        let new_mnt = Self::registered_mount_at(&mounts, new_root)?.clone();
        // Linux resolves `pivot_root(2)` against the caller's root, which may
        // differ from the namespace root after `chroot(2)`.
        let old_mnt = current_root.clone().resolve_final_mount().mnt.clone();
        let root_mount = self.root.clone();

        // The caller's root must overmount the structural root: a bare root
        // mount has nothing to pivot away from.
        if Arc::ptr_eq(&old_mnt, &root_mount) {
            return Err(VfsError::InvalidInput);
        }
        // The surgery below rearranges the namespace root stack, so it requires
        // the caller's visible root to be the namespace's visible root. A
        // caller root moved by `chroot(2)` is not supported yet; reject it
        // instead of rearranging an unrelated mount.
        if !Arc::ptr_eq(&old_mnt, &self.root_path().resolve_final_mount().mnt) {
            return Err(VfsError::InvalidInput);
        }
        // Linux: `new_mnt == root_mnt || old_mnt == root_mnt` -> EBUSY ("loop,
        // on the same file system").
        if Arc::ptr_eq(&new_mnt, &old_mnt) || Arc::ptr_eq(put_old.mount(), &old_mnt) {
            return Err(VfsError::ResourceBusy);
        }
        if !put_old.is_dir() {
            return Err(VfsError::NotADirectory);
        }
        // Linux: `is_path_reachable(old_mnt, old.dentry, &new)`. `put_old`
        // must lie inside `new_root`'s subtree; equality is allowed.
        if !Self::is_reachable_from(put_old, new_root) {
            return Err(VfsError::InvalidInput);
        }

        // Serialize against concurrent mount-tree mutation on every inode the
        // surgery touches. Take the locks in the same total order as the rename
        // path (an ancestor before its descendant, then a stable pointer
        // tiebreak), so a concurrent multi-inode acquisition cannot ABBA against
        // this surgery. Inodes are never taken twice because
        // `lock_namespace_exclusive` is a non-reentrant write lock.
        let new_parent_path = new_mnt.location().ok_or(VfsError::InvalidInput)?;
        // Linux `root_parent`/`root_mp`: the old root's own parent mount and
        // mountpoint, where `new_mnt` takes over. The visible root may sit
        // anywhere in the stack over the structural root, not only at the
        // direct child position.
        let old_parent_path = old_mnt.location().ok_or(VfsError::InvalidInput)?;
        let mut targets: Vec<(Arc<VfsInode>, Vec<usize>)> = Vec::new();
        let mut push_target = |path: &Path| {
            let inode = path.inode();
            if !targets.iter().any(|(held, _)| Arc::ptr_eq(held, &inode)) {
                targets.push((inode, path.dentry.namespace_lock_key()));
            }
        };
        push_target(&old_parent_path);
        push_target(&new_parent_path);
        push_target(put_old);
        targets.sort_by(|left, right| left.1.cmp(&right.1));
        let _guards: Vec<_> = targets
            .iter()
            .map(|(inode, _)| inode.lock_namespace_exclusive())
            .collect();

        // Re-validate under the locks: the mount stacks read while resolving the
        // paths may have changed before these inode locks were acquired. The
        // surgery below assumes the old visible root is attached exactly at its
        // own parent mountpoint, so verify it and fail instead of asserting
        // inside the critical section.
        let (old_parent_mount, old_parent_dentry) =
            (old_parent_path.mnt.clone(), old_parent_path.dentry.clone());
        if !old_parent_mount
            .child_mount_at(&old_parent_dentry)
            .is_some_and(|mounted| Arc::ptr_eq(&mounted, &old_mnt))
        {
            return Err(VfsError::InvalidInput);
        }

        // 1. Detach `new_mnt` from its old parent, restoring whatever mount it
        //    covered at its original location.
        let (new_parent_mount, new_parent_dentry) =
            (new_parent_path.mnt.clone(), new_parent_path.dentry.clone());
        mount_edge_detach(&new_parent_mount, &new_parent_dentry, &new_mnt);

        // 2. Stack `new_mnt` over the old visible root at the old root's own
        //    mountpoint (`attach_mnt(new_mnt, root_parent, root_mp)`),
        //    temporarily holding the old root in its `covers` slot. The
        //    re-validation above guarantees this returns the old visible root.
        *new_mnt.covers.lock() = old_parent_mount.install_child_mount(&old_parent_dentry, &new_mnt);
        *new_mnt.mnt_location.lock() = Some(old_parent_path);

        // 3. Take the old visible root out of the stack. The chain it covered
        //    at the root (usually empty) stays covered under `new_mnt`,
        //    matching Linux's stack-preserving move.
        let chain_below = old_mnt.covers.lock().take();
        old_mnt.detach();
        if let Some(chain) = chain_below {
            *new_mnt.covers.lock() = Some(chain);
        }

        // 4. Attach the old root at `put_old`, preserving any mount stacked
        //    there (it becomes `old_mnt`'s covered layer).
        if let Some(covered) = put_old.mnt.install_child_mount(&put_old.dentry, &old_mnt) {
            *old_mnt.covers.lock() = Some(covered);
        }
        *old_mnt.mnt_location.lock() = Some(put_old.clone());

        drop(mounts);
        Ok(())
    }
}

/// Removes `mount` from `parent`'s child set at `dentry`, restoring whatever
/// mount it covered there, and clears the mount's location pointer.
///
/// Infallible: callers hold the namespace registry lock and the affected inode
/// namespace-exclusive locks (see [`MntNamespace::pivot_root`]).
fn mount_edge_detach(parent: &Mount, dentry: &Dentry, mount: &Arc<Mount>) {
    match mount.covers.lock().take() {
        Some(covered) => {
            parent.install_child_mount(dentry, &covered);
        }
        None => parent.remove_child_mount(dentry),
    }
    mount.detach();
}

/// Result of cloning a mount namespace and retargeting filesystem context paths.
#[derive(Debug)]
pub struct NamespaceClone {
    /// The cloned mount namespace.
    pub namespace: Arc<MntNamespace>,
    /// The old filesystem root path retargeted into the cloned namespace.
    pub root: Path,
    /// The old current working directory path retargeted into the cloned namespace.
    pub pwd: Path,
}

/// `struct mount`.
#[derive(Debug)]
pub struct Mount {
    mnt: VfsMount,
    mnt_location: Mutex<Option<Path>>,
    mnt_mounts: Mutex<HashMap<DentryKey, Weak<Self>>>,
    mnt_id: u64,
    /// Mount that this one covers (for overmount).
    ///
    /// When a new mount is created at a location that already has a mount,
    /// the old mount is stored here so it can be restored on unmount.
    covers: Mutex<Option<Arc<Self>>>,
    mnt_devname: Option<String>,
}

impl Mount {
    /// Creates a root mount with per-mount flags and device name.
    pub fn new_root_with_flags_and_devname(
        fs: &Arc<SuperBlock>,
        flags: MountFlags,
        devname: Option<&str>,
    ) -> Arc<Self> {
        Arc::new(Self::new_detached(fs, flags, devname))
    }

    fn new_detached(fs: &Arc<SuperBlock>, flags: MountFlags, devname: Option<&str>) -> Self {
        Self::try_new_detached(fs, flags, devname, None)
            .expect("unchecked mount activation cannot reject flags")
            .expect("a dying or dead superblock cannot be mounted")
    }

    fn try_new_detached(
        fs: &Arc<SuperBlock>,
        flags: MountFlags,
        devname: Option<&str>,
        requested_superblock_flags: Option<SuperBlockFlags>,
    ) -> VfsResult<Option<Self>> {
        let Some(mnt) = VfsMount::try_new(fs, flags, requested_superblock_flags)? else {
            return Ok(None);
        };
        Ok(Some(Self {
            mnt,
            mnt_location: Mutex::default(),
            mnt_mounts: Mutex::default(),
            mnt_id: MOUNT_ID_COUNTER.fetch_add(1, Ordering::Relaxed),
            covers: Mutex::default(),
            mnt_devname: devname.map(|s| s.to_string()),
        }))
    }

    fn clone_mnt(source: &Path) -> Self {
        Self {
            mnt: VfsMount::clone_from_path(source),
            mnt_location: Mutex::default(),
            mnt_mounts: Mutex::default(),
            mnt_id: MOUNT_ID_COUNTER.fetch_add(1, Ordering::Relaxed),
            covers: Mutex::default(),
            mnt_devname: source.mount().mnt_devname.clone(),
        }
    }

    fn into_arc_at(mut self, location_in_parent: Option<Path>) -> Arc<Self> {
        self.mnt_location = Mutex::new(location_in_parent);
        Arc::new(self)
    }

    /// Creates a root mount for a filesystem.
    pub fn new_root(fs: &Arc<SuperBlock>) -> Arc<Self> {
        Self::new_root_with_flags_and_devname(fs, MountFlags::empty(), None)
    }

    /// Creates a root mount with per-mount flags.
    pub fn new_root_with_flags(fs: &Arc<SuperBlock>, flags: MountFlags) -> Arc<Self> {
        Self::new_root_with_flags_and_devname(fs, flags, None)
    }

    /// Return a `Path` representing the mount root.
    pub fn root_path(self: &Arc<Self>) -> Path {
        Path::new(self.clone(), self.mnt.mnt_root.clone())
    }

    /// Returns the superblock mounted at this mount object.
    pub fn super_block(&self) -> &Arc<SuperBlock> {
        &self.mnt.mnt_sb
    }

    /// Returns the VFS-wide flags for this mount's superblock.
    pub fn super_block_flags(&self) -> SuperBlockFlags {
        self.super_block().flags()
    }

    /// Returns the filesystem type name for this mount.
    pub fn filesystem_name(&self) -> &str {
        self.super_block().name()
    }

    /// Returns filesystem statistics for this mount.
    pub fn filesystem_stat(&self) -> VfsResult<StatFs> {
        self.super_block().stat()
    }

    /// Allocates an opened pseudo file on this mount.
    pub fn alloc_file_pseudo(
        self: &Arc<Self>,
        inode: Arc<VfsInode>,
        name: &str,
        flags: FMode,
        open_flags: OpenFlags,
        f_op: Arc<dyn FileOperations>,
        cred: Arc<Cred>,
    ) -> VfsResult<Arc<VfsFile>> {
        let dentry = Dentry::new_file_from_inode(inode, None, name.to_string());
        dentry.bind_super_block(self.super_block());
        let path = Path::new(self.clone(), dentry);
        let mut file = VfsFileBuilder::from_path_state(path, flags, open_flags, f_op, cred);
        file.mark_opened()?;
        file.finish()
    }

    /// Returns the path in the parent mount.
    pub fn location(&self) -> Option<Path> {
        self.mnt_location.lock().clone()
    }

    /// Returns whether this mount has no parent mount.
    pub fn is_root(&self) -> bool {
        self.mnt_location.lock().is_none()
    }

    /// Returns the mount namespace identity assigned to this mount object.
    pub fn mount_id(self: &Arc<Self>) -> u64 {
        self.mnt_id
    }

    /// Returns the device name for this mount, if set.
    pub fn devname(&self) -> Option<&str> {
        self.mnt_devname.as_deref()
    }

    /// Returns the temporary VFS device identity exposed for inode metadata.
    ///
    /// X-Kernel does not yet have a superblock-level `s_dev` model, so mounted
    /// filesystems use the mount ID as a stable synthetic device number.
    pub fn synthetic_device_id(self: &Arc<Self>) -> u64 {
        self.mnt_id
    }

    /// Returns this mount's flags.
    pub fn flags(&self) -> MountFlags {
        self.mnt.flags()
    }

    fn set_flags(&self, flags: MountFlags) {
        self.mnt.set_flags(flags);
    }

    /// Returns whether this mountpoint is mounted read-only.
    pub fn is_readonly(&self) -> bool {
        self.flags().contains(MountFlags::RDONLY)
    }

    /// Returns a snapshot of direct child mounts currently attached here.
    pub fn children(self: &Arc<Self>) -> Vec<Arc<Self>> {
        self.mnt_mounts
            .lock()
            .values()
            .filter_map(Weak::upgrade)
            .collect()
    }

    fn covered_mount(&self) -> Option<Arc<Self>> {
        self.covers.lock().clone()
    }

    fn collect_subtree(self: &Arc<Self>) -> Vec<Arc<Self>> {
        let mut mounts = vec![self.clone()];
        let mut index = 0;
        while index < mounts.len() {
            let mount = mounts[index].clone();
            for child in mount.children() {
                let mut layer = Some(child);
                while let Some(mount) = layer {
                    layer = mount.covered_mount();
                    mounts.push(mount);
                }
            }
            index += 1;
        }
        mounts
    }

    fn child_mount_at(&self, dentry: &Dentry) -> Option<Arc<Self>> {
        let key = dentry.key();
        let mut mounts = self.mnt_mounts.lock();
        let mount = mounts.get(&key).and_then(Weak::upgrade);
        if mount.is_none() {
            mounts.remove(&key);
        }
        mount
    }

    fn install_child_mount(&self, mountpoint: &Dentry, mount: &Arc<Self>) -> Option<Arc<Self>> {
        self.mnt_mounts
            .lock()
            .insert(mountpoint.key(), Arc::downgrade(mount))
            .and_then(|old| old.upgrade())
    }

    fn remove_child_mount(&self, mountpoint: &Dentry) {
        self.mnt_mounts.lock().remove(&mountpoint.key());
    }

    fn detach(&self) {
        self.mnt_location.lock().take();
    }
}

/// A resolved location within a mountpoint.
#[derive(Debug, Clone)]
pub struct Path {
    mnt: Arc<Mount>,
    dentry: Dentry,
}

/// A pathname rendered relative to a process root.
#[derive(Debug, Eq, PartialEq)]
pub enum RenderedPath {
    /// The location is reachable below the requested root.
    Reachable(PathBuf),
    /// The location is outside the requested root.
    Unreachable(PathBuf),
}

impl Path {
    /// Create a `struct path` from a mount and dentry.
    pub fn new(mnt: Arc<Mount>, dentry: Dentry) -> Self {
        Self { mnt, dentry }
    }

    pub(crate) fn with_dentry(&self, dentry: Dentry) -> Self {
        Self::new(self.mnt.clone(), dentry)
    }

    /// Returns the mount containing this path.
    pub fn mount(&self) -> &Arc<Mount> {
        &self.mnt
    }

    fn is_mount_root(&self) -> bool {
        if !self.dentry.ptr_eq(&self.mnt.mnt.mnt_root) {
            return false;
        }

        self.mnt.location().is_none_or(|parent_path| {
            parent_path
                .mnt
                .child_mount_at(&parent_path.dentry)
                .is_some_and(|mount| Arc::ptr_eq(&mount, &self.mnt))
        })
    }

    /// Returns the superblock containing this location.
    pub(crate) fn super_block(&self) -> &Arc<SuperBlock> {
        self.mnt.super_block()
    }

    /// Returns filesystem statistics for this path.
    pub fn filesystem_stat(&self) -> VfsResult<StatFs> {
        self.super_block().stat()
    }

    /// Synchronizes the filesystem containing this path.
    pub fn sync_filesystem(&self) -> VfsResult<()> {
        self.super_block().sync_fs()
    }

    /// Returns the maximum regular-file size allowed on this path.
    pub fn max_file_size(&self) -> u64 {
        self.super_block().max_file_size()
    }

    /// Returns this path's inode identity.
    pub fn inode(&self) -> Arc<VfsInode> {
        self.dentry.vfs_inode()
    }

    /// Returns a stable key for this path's inode identity.
    pub fn inode_key(&self) -> usize {
        Arc::as_ptr(&self.inode()) as usize
    }

    /// Returns a weak reference to this path's inode identity.
    pub fn weak_inode(&self) -> Weak<VfsInode> {
        Arc::downgrade(&self.inode())
    }

    /// Returns this path's node type.
    pub fn node_type(&self) -> NodeType {
        self.inode().node_type()
    }

    /// Returns whether this path names a directory.
    pub fn is_dir(&self) -> bool {
        self.node_type() == NodeType::Directory
    }

    /// Returns whether this path names a regular file.
    pub fn is_regular_file(&self) -> bool {
        self.node_type() == NodeType::RegularFile
    }

    /// Returns whether this path names a non-directory file.
    pub fn is_file(&self) -> bool {
        !self.is_dir()
    }

    /// Returns whether this path names a symbolic link.
    pub fn is_symlink(&self) -> bool {
        self.node_type() == NodeType::Symlink
    }

    /// Attempt to downcast this path's inode-private state.
    pub fn downcast_node<T: core::any::Any + Send + Sync>(&self) -> VfsResult<Arc<T>> {
        self.inode().downcast()
    }

    /// Returns the underlying directory entry.
    pub(crate) fn dentry(&self) -> &Dentry {
        &self.dentry
    }

    /// Returns a cached child inode or inserts a filesystem-created child.
    pub fn cached_child_inode_or_insert_with(
        &self,
        name: &str,
        create: impl FnOnce(&Dentry, String) -> Dentry,
    ) -> u64 {
        if let Some(entry) = self
            .dentry
            .lookup_cache(name)
            .filter(Dentry::is_really_positive)
        {
            return entry.inode();
        }

        let name = name.to_string();
        let entry = create(&self.dentry, name.clone());
        self.dentry
            .insert_cache(name, entry.clone())
            .filter(Dentry::is_really_positive)
            .unwrap_or(entry)
            .inode()
    }

    /// Returns metadata for the inode referenced by this location.
    pub fn metadata(&self) -> Metadata {
        self.dentry.vfs_inode().metadata()
    }

    /// Checks access to this location's inode.
    pub fn permission(&self, permission: Permission, cred: &Cred) -> VfsResult<()> {
        self.inode().permission(permission, cred)
    }

    /// Returns a snapshot of this location's name within its parent directory.
    pub fn name(&self) -> String {
        self.dentry.name_snapshot()
    }

    /// Returns the parent location, if any.
    pub fn parent(&self) -> Option<Self> {
        if !self.dentry.ptr_eq(&self.mnt.mnt.mnt_root) {
            return Some(self.with_dentry(self.dentry.parent()?));
        }
        self.mnt.location()?.parent()
    }

    /// Returns `true` if this is the root of a mount tree.
    pub fn is_root(&self) -> bool {
        self.mnt.is_root() && self.dentry.is_root_of_mount()
    }

    /// Build the absolute path for this location.
    pub fn absolute_path(&self) -> VfsResult<PathBuf> {
        if !self.dentry.ptr_eq(&self.mnt.mnt.mnt_root)
            && let Some(dynamic_name) = self.dentry.dynamic_name()?
        {
            return Ok(PathBuf::from(dynamic_name));
        }

        let (_, components) = self.render_components(None)?;
        Ok(Self::components_to_path(&components))
    }

    /// Renders this location relative to `root` with mount-aware reachability.
    pub fn render_from(&self, root: &Self) -> VfsResult<RenderedPath> {
        let (is_reachable, components) = self.render_components(Some(root))?;
        let path = Self::components_to_path(&components);
        Ok(if is_reachable {
            RenderedPath::Reachable(path)
        } else {
            RenderedPath::Unreachable(path)
        })
    }

    /// Builds this location's path below `root`.
    ///
    /// Returns [`VfsError::NotFound`] when the location is outside `root`.
    pub fn path_below(&self, root: &Self) -> VfsResult<PathBuf> {
        match self.render_from(root)? {
            RenderedPath::Reachable(path) => Ok(path),
            RenderedPath::Unreachable(_) => Err(VfsError::NotFound),
        }
    }

    fn render_components(&self, root: Option<&Self>) -> VfsResult<(bool, Vec<String>)> {
        let mut components = Vec::new();
        let mut current = self.clone();
        loop {
            if root.is_some_and(|root| current.ptr_eq(root)) {
                return Ok((true, components));
            }

            if !current.dentry.ptr_eq(&current.mnt.mnt.mnt_root) {
                components.push(current.name());
                let Some(parent) = current.dentry.parent() else {
                    return Ok((root.is_none(), components));
                };
                current = current.with_dentry(parent);
                continue;
            }

            let Some(mountpoint) = current.mnt.location() else {
                return Ok((root.is_none(), components));
            };
            if root.is_some_and(|root| mountpoint.ptr_eq(root)) {
                return Ok((true, components));
            }
            components.push(mountpoint.name());
            let Some(parent) = mountpoint.parent() else {
                return Ok((root.is_none(), components));
            };
            current = parent;
        }
    }

    fn components_to_path(components: &[String]) -> PathBuf {
        iter::once("/")
            .chain(components.iter().map(String::as_str).rev())
            .collect()
    }

    /// Returns a display pathname for this location.
    pub fn display_path(&self) -> VfsResult<String> {
        Ok(self.absolute_path()?.to_string())
    }

    /// Returns `true` if this location references the same entry.
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.mnt, &other.mnt) && self.dentry.ptr_eq(&other.dentry)
    }

    /// Returns `true` if both locations point at the same VFS inode identity.
    pub fn is_same_inode(&self, other: &Self) -> bool {
        self.dentry.is_same_inode(&other.dentry)
    }

    /// Returns whether this path's mount is read-only.
    pub fn is_mount_readonly(&self) -> bool {
        self.mnt.is_readonly()
    }

    /// Returns whether this path is effectively read-only.
    pub fn is_effectively_readonly(&self) -> bool {
        self.is_mount_readonly() || self.super_block().is_readonly()
    }

    /// Ensures this path's mount allows modification.
    pub fn check_writable_mount(&self) -> VfsResult<()> {
        if self.is_effectively_readonly() {
            return Err(VfsError::ReadOnlyFilesystem);
        }
        Ok(())
    }

    pub(crate) fn touch_atime(&self) {
        if self.is_effectively_readonly() {
            return;
        }

        let mount_flags = self.mnt.flags();
        let super_block_flags = self.super_block().flags();
        if mount_flags.contains(MountFlags::NOATIME)
            || super_block_flags.contains(SuperBlockFlags::NOATIME)
            || (self.inode().node_type() == NodeType::Directory
                && (mount_flags.contains(MountFlags::NODIRATIME)
                    || super_block_flags.contains(SuperBlockFlags::NODIRATIME)))
        {
            return;
        }

        let inode = self.inode();
        let now = inode.current_time();
        let times = inode.times();
        if mount_flags.contains(MountFlags::RELATIME)
            && times.modified_at < times.accessed_at
            && times.changed_at < times.accessed_at
            && now
                .duration_since(times.accessed_at)
                .unwrap_or(TimeSpan::ZERO)
                < TimeSpan::from_secs(24 * 60 * 60)
        {
            return;
        }
        if times.accessed_at == now {
            return;
        }

        // Linux treats atime persistence as best effort so a read that already
        // completed is never converted into an update-time failure.
        let _ = inode.update_time(self.dentry(), now, InodeUpdateTime::Access);
    }

    pub(crate) fn update_cmtime(&self) -> VfsResult<()> {
        if self.is_effectively_readonly() {
            return Ok(());
        }

        let inode = self.inode();
        let now = inode.current_time();
        let times = inode.times();
        if times.modified_at == now && times.changed_at == now {
            return Ok(());
        }
        inode.update_time(self.dentry(), now, InodeUpdateTime::ChangeAndModification)
    }

    /// Changes this inode's owner after applying Linux chown/chgrp authorization.
    pub fn chown(&self, uid: Option<u32>, gid: Option<u32>, cred: &Cred) -> VfsResult<()> {
        if uid.is_none() && gid.is_none() {
            return Ok(());
        }

        self.check_writable_mount()?;
        let metadata = self.metadata();
        let is_owner = cred.fsuid() == metadata.uid;
        let is_privileged = cred.has_cap_chown();

        if uid.is_some_and(|uid| !is_privileged && (!is_owner || uid != metadata.uid)) {
            return Err(VfsError::OperationNotPermitted);
        }
        if gid.is_some_and(|gid| {
            !is_privileged && (!is_owner || (gid != metadata.gid && !cred.in_group(gid)))
        }) {
            return Err(VfsError::OperationNotPermitted);
        }

        let mut mode = metadata.mode.permission();
        mode.remove(NodePermission::SET_UID);
        if mode.contains(NodePermission::GROUP_EXEC) {
            mode.remove(NodePermission::SET_GID);
        }
        self.dentry.update_metadata(MetadataUpdate {
            owner: Some((uid.unwrap_or(metadata.uid), gid.unwrap_or(metadata.gid))),
            mode: Some(mode),
            ..Default::default()
        })
    }

    /// Changes this inode's permission bits after applying Linux chmod authorization.
    pub fn chmod(&self, mut mode: NodePermission, cred: &Cred) -> VfsResult<()> {
        self.check_writable_mount()?;
        let metadata = self.metadata();
        if cred.fsuid() != metadata.uid && !cred.has_cap_fowner() {
            return Err(VfsError::OperationNotPermitted);
        }
        if mode.contains(NodePermission::SET_GID)
            && !cred.in_group(metadata.gid)
            && !cred.has_cap_fowner()
        {
            mode.remove(NodePermission::SET_GID);
        }
        self.dentry.update_metadata(MetadataUpdate {
            mode: Some(mode),
            ..Default::default()
        })
    }

    /// Changes this inode's timestamps after applying Linux utimens authorization.
    pub fn set_times(
        &self,
        atime: Option<SetattrTime>,
        mtime: Option<SetattrTime>,
        cred: &Cred,
    ) -> VfsResult<()> {
        if atime.is_none() && mtime.is_none() {
            return Ok(());
        }

        self.check_writable_mount()?;
        let metadata = self.metadata();
        let is_owner = cred.fsuid() == metadata.uid;
        let is_touch = matches!(
            (atime, mtime),
            (Some(SetattrTime::Current(_)), Some(SetattrTime::Current(_)))
        );
        if !is_touch {
            if !is_owner && !cred.has_cap_fowner() {
                return Err(VfsError::OperationNotPermitted);
            }
        } else if !is_owner && !cred.has_cap_fowner() {
            self.permission(Permission::MAY_WRITE, cred)?;
        }

        self.dentry.update_metadata(MetadataUpdate {
            atime: atime.map(SetattrTime::value),
            mtime: mtime.map(SetattrTime::value),
            ..Default::default()
        })
    }

    /// Returns metadata without security checks.
    pub fn getattr_nosec(
        &self,
        request_mask: GetattrRequestMask,
        query_flags: GetattrQueryFlags,
    ) -> VfsResult<Metadata> {
        self.inode().getattr(self, request_mask, query_flags)
    }

    /// Returns VFS metadata for this path.
    pub fn getattr(&self) -> VfsResult<Metadata> {
        let mut metadata =
            self.getattr_nosec(GetattrRequestMask::empty(), GetattrQueryFlags::empty())?;
        metadata.device = self.mnt.synthetic_device_id();
        Ok(metadata)
    }

    /// Returns `true` if this path is a mountpoint directory.
    pub fn is_mountpoint(&self) -> bool {
        self.dentry.as_dir().is_ok() && self.mnt.child_mount_at(&self.dentry).is_some()
    }

    fn check_not_mountpoint(&self) -> VfsResult<()> {
        if self.is_mountpoint() {
            return Err(VfsError::ResourceBusy);
        }
        Ok(())
    }

    fn may_modify_directory(&self, cred: &Cred) -> VfsResult<()> {
        self.permission(Permission::MAY_WRITE | Permission::MAY_EXEC, cred)
    }

    pub(crate) fn vfs_create(
        &self,
        candidate: &Dentry,
        mode: Umode,
        umask: NodePermission,
        exclusive: bool,
        cred: &Cred,
    ) -> VfsResult<()> {
        self.check_writable_mount()?;
        self.may_modify_directory(cred)?;
        let dir_inode = self.inode();
        let mode = dir_inode.prepare_create_mode(
            mode,
            umask,
            NodePermission::all(),
            NodeType::RegularFile,
            cred,
        );
        dir_inode.create_with_mode(candidate, mode, exclusive, cred)
    }

    pub(crate) fn vfs_mknod(
        &self,
        candidate: &Dentry,
        mode: Umode,
        device: DeviceId,
        umask: NodePermission,
        cred: &Cred,
    ) -> VfsResult<()> {
        self.check_writable_mount()?;
        self.may_modify_directory(cred)?;
        let node_type = mode.node_type();
        if matches!(node_type, NodeType::CharacterDevice | NodeType::BlockDevice)
            && !cred.is_privileged()
        {
            return Err(VfsError::OperationNotPermitted);
        }
        let dir_inode = self.inode();
        let mode = dir_inode.prepare_create_mode(mode, umask, mode.permission(), node_type, cred);
        dir_inode.mknod_with_mode(candidate, mode, device, cred)
    }

    pub(crate) fn vfs_mkdir(
        &self,
        candidate: &Dentry,
        permission: NodePermission,
        umask: NodePermission,
        cred: &Cred,
    ) -> VfsResult<()> {
        self.check_writable_mount()?;
        self.may_modify_directory(cred)?;
        let dir_inode = self.inode();
        let allowed_permission =
            NodePermission::all() & !(NodePermission::SET_UID | NodePermission::SET_GID);
        let mode = dir_inode.prepare_create_mode(
            Umode::new(NodeType::Directory, permission),
            umask,
            allowed_permission,
            NodeType::Directory,
            cred,
        );
        dir_inode.mkdir(candidate, mode, cred)
    }

    pub(crate) fn vfs_symlink(
        &self,
        candidate: &Dentry,
        target: &str,
        cred: &Cred,
    ) -> VfsResult<()> {
        self.check_writable_mount()?;
        self.may_modify_directory(cred)?;
        let dir_inode = self.inode();
        dir_inode.symlink(candidate, target, cred)
    }

    pub(crate) fn vfs_link(&self, candidate: &Dentry, source: &Self, cred: &Cred) -> VfsResult<()> {
        if !Arc::ptr_eq(&self.mnt, &source.mnt) {
            return Err(VfsError::CrossesDevices);
        }
        self.check_writable_mount()?;
        self.may_modify_directory(cred)?;
        if source.is_dir() {
            return Err(VfsError::OperationNotPermitted);
        }
        let source_inode = source.inode();
        let _source_guard = source_inode.lock_namespace_exclusive();
        self.inode().link(candidate, &source.dentry)
    }

    fn check_sticky(&self, victim: &Self, cred: &Cred) -> VfsResult<()> {
        let dir = self.metadata();
        if !dir.mode.permission().contains(NodePermission::STICKY) {
            return Ok(());
        }

        let fsuid = cred.fsuid();
        if fsuid == 0 || fsuid == dir.uid || fsuid == victim.metadata().uid {
            Ok(())
        } else {
            Err(VfsError::OperationNotPermitted)
        }
    }

    /// Creates a regular file under this directory path.
    ///
    /// The final lookup and authorization are serialized by the parent
    /// directory namespace lock, and the filesystem receives an exclusive
    /// create request.
    pub fn create(&self, name: &str, permission: NodePermission, cred: &Cred) -> VfsResult<Self> {
        let mode = Umode::new(NodeType::RegularFile, permission);
        self.dentry
            .as_dir()?
            .create_exclusive_with(name, |candidate| {
                self.vfs_create(candidate, mode, NodePermission::empty(), true, cred)
            })
            .map(|entry| self.with_dentry(entry))
    }

    /// Creates a directory under this directory path.
    pub fn mkdir(&self, name: &str, permission: NodePermission, cred: &Cred) -> VfsResult<Self> {
        self.dentry
            .as_dir()?
            .create_exclusive_with(name, |candidate| {
                self.vfs_mkdir(candidate, permission, NodePermission::empty(), cred)
            })
            .map(|entry| self.with_dentry(entry))
    }

    /// Creates a non-regular filesystem node under this directory path.
    ///
    /// Character and block device creation requires a privileged credential.
    /// The existence check takes precedence over create-only authorization
    /// failures.
    pub fn mknod(
        &self,
        name: &str,
        node_type: NodeType,
        permission: NodePermission,
        device: DeviceId,
        cred: &Cred,
    ) -> VfsResult<Self> {
        let mode = Umode::new(node_type, permission);
        self.dentry
            .as_dir()?
            .create_exclusive_with(name, |candidate| {
                self.vfs_mknod(candidate, mode, device, NodePermission::empty(), cred)
            })
            .map(|entry| self.with_dentry(entry))
    }

    /// Creates a hard link to an existing path.
    pub fn link(&self, name: &str, source: &Self, cred: &Cred) -> VfsResult<Self> {
        self.dentry
            .as_dir()?
            .create_exclusive_with(name, |candidate| self.vfs_link(candidate, source, cred))
            .map(|entry| self.with_dentry(entry))
    }

    /// Creates a symbolic link under this directory path.
    pub fn symlink(&self, name: &str, target: &str, cred: &Cred) -> VfsResult<Self> {
        self.dentry
            .as_dir()?
            .create_exclusive_with(name, |candidate| self.vfs_symlink(candidate, target, cred))
            .map(|entry| self.with_dentry(entry))
    }

    /// Rename an entry within the same mountpoint.
    pub fn rename(
        &self,
        old_name: &str,
        new_dir: &Self,
        new_name: &str,
        flags: RenameFlags,
        cred: &Cred,
    ) -> VfsResult<()> {
        if !Arc::ptr_eq(&self.mnt, &new_dir.mnt) {
            return Err(VfsError::CrossesDevices);
        }
        self.check_writable_mount()?;

        self.dentry.as_dir()?.rename_with(
            old_name,
            new_dir.dentry.as_dir()?,
            new_name,
            flags,
            |source, target| {
                self.may_modify_directory(cred)?;
                if !self.ptr_eq(new_dir) {
                    new_dir.may_modify_directory(cred)?;
                }

                let old_path = self.with_dentry(source.clone());
                self.check_sticky(&old_path, cred)?;
                old_path.check_not_mountpoint()?;
                if target.is_really_positive() {
                    let new_path = new_dir.with_dentry(target.clone());
                    new_dir.check_sticky(&new_path, cred)?;
                    new_path.check_not_mountpoint()?;
                }
                Ok(())
            },
        )
    }

    /// Remove a non-directory entry.
    pub fn unlink(&self, name: &str, cred: &Cred) -> VfsResult<()> {
        self.unlink_with_pathname(name, false, cred)
    }

    pub(crate) fn unlink_with_pathname(
        &self,
        name: &str,
        has_trailing_slash: bool,
        cred: &Cred,
    ) -> VfsResult<()> {
        self.check_writable_mount()?;
        self.dentry.as_dir()?.unlink_with(name, |victim| {
            if has_trailing_slash {
                return Err(VfsError::NotADirectory);
            }
            self.may_modify_directory(cred)?;
            let victim_path = self.with_dentry(victim.clone());
            self.check_sticky(&victim_path, cred)?;
            victim_path.check_not_mountpoint()
        })
    }

    /// Remove a directory entry.
    pub fn rmdir(&self, name: &str, cred: &Cred) -> VfsResult<()> {
        self.check_writable_mount()?;
        self.dentry.as_dir()?.rmdir_with(name, |victim| {
            self.may_modify_directory(cred)?;
            let victim_path = self.with_dentry(victim.clone());
            self.check_sticky(&victim_path, cred)?;
            victim_path.check_not_mountpoint()
        })
    }

    /// Mount a filesystem at this path.
    fn mount_filesystem_with_requested_superblock_flags(
        &self,
        fs: &Arc<SuperBlock>,
        flags: MountFlags,
        devname: Option<&str>,
        requested_superblock_flags: Option<SuperBlockFlags>,
    ) -> VfsResult<Arc<Mount>> {
        let mount = Mount::try_new_detached(fs, flags, devname, requested_superblock_flags)?
            .ok_or(VfsError::ResourceBusy)?;
        self.graft_tree(mount)
    }

    fn mount_bind(&self, source: &Path) -> VfsResult<Arc<Mount>> {
        self.graft_tree(Mount::clone_mnt(source))
    }

    fn graft_tree(&self, mount: Mount) -> VfsResult<Arc<Mount>> {
        let inode = self.inode();
        let _namespace_guard = inode.lock_namespace_exclusive();
        if mount.mnt.mnt_root.as_dir().is_ok() != self.is_dir() {
            // Linux `graft_tree()` returns `ENOTDIR` for either direction of
            // a directory/non-directory mismatch.
            return Err(VfsError::NotADirectory);
        }
        let result = mount.into_arc_at(Some(self.clone()));
        if let Some(old) = self.mnt.install_child_mount(&self.dentry, &result) {
            *result.covers.lock() = Some(old);
        }
        Ok(result)
    }

    /// Unmount the filesystem rooted at this path.
    fn unmount(&self) -> VfsResult<()> {
        if !self.dentry.ptr_eq(&self.mnt.mnt.mnt_root) {
            return Err(VfsError::InvalidInput);
        }
        let parent_path = self.mnt.location().ok_or(VfsError::InvalidInput)?;
        let inode = parent_path.inode();
        let _namespace_guard = inode.lock_namespace_exclusive();
        if !parent_path
            .mnt
            .child_mount_at(&parent_path.dentry)
            .is_some_and(|mount| Arc::ptr_eq(&mount, &self.mnt))
        {
            return Err(VfsError::InvalidInput);
        }
        if !self.mnt.mnt_mounts.lock().is_empty() {
            return Err(VfsError::ResourceBusy);
        }

        let covered = self.mnt.covers.lock().take();
        match covered {
            Some(ref mount) => {
                parent_path
                    .mnt
                    .install_child_mount(&parent_path.dentry, mount);
            }
            None => {
                parent_path.mnt.remove_child_mount(&parent_path.dentry);
            }
        }
        self.mnt.detach();
        Ok(())
    }

    /// Recursively unmount this filesystem and all children.
    #[cfg(unittest)]
    fn unmount_tree(&self) -> VfsResult<()> {
        let mounts = self.mnt.collect_subtree();
        self.detach_mount_subtree(&mounts)
    }

    fn detach_mount_subtree(&self, mounts: &[Arc<Mount>]) -> VfsResult<()> {
        if !self.dentry.ptr_eq(&self.mnt.mnt.mnt_root) {
            return Err(VfsError::InvalidInput);
        }
        if !mounts
            .first()
            .is_some_and(|root| Arc::ptr_eq(root, &self.mnt))
        {
            return Err(VfsError::InvalidInput);
        }

        let parent_path = self.mnt.location().ok_or(VfsError::InvalidInput)?;
        let inode = parent_path.inode();
        let _namespace_guard = inode.lock_namespace_exclusive();
        if !parent_path
            .mnt
            .child_mount_at(&parent_path.dentry)
            .is_some_and(|mount| Arc::ptr_eq(&mount, &self.mnt))
        {
            return Err(VfsError::InvalidInput);
        }

        let covered = self.mnt.covers.lock().take();
        match covered {
            Some(ref mount) => {
                parent_path
                    .mnt
                    .install_child_mount(&parent_path.dentry, mount);
            }
            None => parent_path.mnt.remove_child_mount(&parent_path.dentry),
        }

        // All fallible checks complete before this commit phase. The namespace
        // entry point also validates registry membership for every layer. The
        // remaining operations are infallible, so an overmount cannot be
        // restored halfway and leave stale registry entries.
        for mount in mounts {
            mount.mnt_mounts.lock().clear();
            mount.covers.lock().take();
            mount.detach();
        }
        Ok(())
    }

    /// Changes a regular file length through the VFS truncate path.
    pub fn truncate(&self, len: u64, cred: &Cred) -> VfsResult<()> {
        self.permission(Permission::MAY_WRITE, cred)?;
        self.truncate_opened(len)
    }

    pub(crate) fn truncate_opened(&self, len: u64) -> VfsResult<()> {
        self.check_writable_mount()?;
        if self.is_dir() {
            return Err(VfsError::IsADirectory);
        }
        if !self.is_regular_file() {
            // Only regular files have a truncatable size. Linux returns EINVAL
            // for pipes, sockets, devices and other special files, so refuse
            // the length change at the common truncate boundary instead of
            // letting a resident metadata update publish a fake `i_size`.
            return Err(VfsError::InvalidInput);
        }

        let inode = self.inode();
        if len > self.max_file_size() {
            return Err(VfsError::FileTooLarge);
        }
        if !inode.flags().contains(NodeFlags::NON_CACHEABLE) {
            inode.set_len(len)?;
        } else {
            self.dentry.update_metadata(MetadataUpdate {
                size: Some(len),
                ..Default::default()
            })?;
        }
        Ok(())
    }

    /// Resolves overmounts visible from this path, returning the path bound to
    /// the topmost covering mount.
    ///
    /// `umount` acts on the mounted path, so it resolves this before detaching;
    /// `pivot_root` deliberately keeps the unresolved path for the relocated
    /// filesystem root.
    pub fn resolve_final_mount(self) -> Self {
        let mut path = self;
        while let Some(mountpoint) = path.mnt.child_mount_at(&path.dentry) {
            let entry = mountpoint.mnt.mnt_root.clone();
            path = Self::new(mountpoint, entry);
        }
        path
    }
}

fn clone_mount_tree(
    source: &Arc<Mount>,
    location_in_parent: Option<Path>,
    mount_map: &mut HashMap<usize, Arc<Mount>>,
) -> Arc<Mount> {
    let cloned = Mount::clone_mnt(&source.root_path()).into_arc_at(location_in_parent);
    mount_map.insert(Arc::as_ptr(source) as usize, cloned.clone());

    if let Some(covered) = source.covered_mount() {
        let location = cloned.location();
        let cloned_covered = clone_mount_tree(&covered, location, mount_map);
        *cloned.covers.lock() = Some(cloned_covered);
    }

    for child in source.children() {
        let mountpoint = child
            .location()
            .expect("child mount must have mountpoint")
            .dentry;
        let location = Path::new(cloned.clone(), mountpoint.clone());
        let cloned_child = clone_mount_tree(&child, Some(location), mount_map);
        cloned.install_child_mount(&mountpoint, &cloned_child);
    }

    cloned
}

fn retarget_path(path: &Path, mount_map: &HashMap<usize, Arc<Mount>>) -> VfsResult<Path> {
    let key = Arc::as_ptr(path.mount()) as usize;
    let mount = mount_map.get(&key).ok_or(VfsError::InvalidInput)?;
    Ok(Path::new(mount.clone(), path.dentry.clone()))
}

unittest::path_mod_test!(tests, "../ut-tests/mount.rs");

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! tmpfs/shmem-style anonymous regular-file helpers.

use alloc::{string::String, sync::Arc};

use ksync::Mutex;
use kvfs::{Mount, NodePermission, Path, SuperBlock, VfsFile, VfsResult, dentry_open};
use linux_raw_sys::general::{
    F_SEAL_FUTURE_WRITE, F_SEAL_GROW, F_SEAL_SEAL, F_SEAL_SHRINK, F_SEAL_WRITE, O_CREAT, O_EXCL,
    O_RDWR,
};

use crate::{MemoryFs, MemoryNode, TMPFS_MAGIC};

bitflags::bitflags! {
    /// Memfd seal bits stored on a shmem inode.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct ShmemSealSet: u32 {
        /// Prevent adding any more seals.
        const SEAL = F_SEAL_SEAL;
        /// Prevent shrinking the file.
        const SHRINK = F_SEAL_SHRINK;
        /// Prevent growing the file.
        const GROW = F_SEAL_GROW;
        /// Prevent writes and writable shared mappings.
        const WRITE = F_SEAL_WRITE;
        /// Prevent future writable shared mappings.
        const FUTURE_WRITE = F_SEAL_FUTURE_WRITE;
    }
}

/// Creates a tmpfs superblock.
pub fn new_tmpfs(mount_flags: kvfs::StatFsFlags) -> Arc<SuperBlock> {
    MemoryFs::new_with_name_flags_and_root_mode(
        "tmpfs",
        TMPFS_MAGIC,
        mount_flags,
        NodePermission::from_bits_truncate(0o1777),
    )
}

/// Kind of shmem object represented by a tmpfs-backed inode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShmemObjectKind {
    /// Anonymous file created for `memfd_create`.
    Memfd,
    /// Kernel-created shared-memory file used by SysV shm or similar users.
    Kernel,
}

/// Inode-scoped state for tmpfs/shmem-style objects.
///
/// This state intentionally stores shmem policy metadata only. File contents
/// and object identity remain owned by the inode-scoped page-cache mapping.
#[derive(Debug)]
pub(super) struct ShmemObjectState {
    _kind: ShmemObjectKind,
    _debug_name: String,
    seals: Mutex<ShmemSealSet>,
    shared_pages: Mutex<usize>,
    writable_shared_pages: Mutex<usize>,
}

impl ShmemObjectState {
    /// Creates policy state for a shmem inode.
    fn new(kind: ShmemObjectKind, debug_name: String, initial_seals: ShmemSealSet) -> Self {
        Self {
            _kind: kind,
            _debug_name: debug_name,
            seals: Mutex::new(initial_seals),
            shared_pages: Mutex::new(0),
            writable_shared_pages: Mutex::new(0),
        }
    }

    /// Returns the current memfd seal bitmask.
    fn seals(&self) -> ShmemSealSet {
        *self.seals.lock()
    }

    /// Returns the current memfd seal bitmask as ABI bits.
    fn seal_bits(&self) -> u32 {
        self.seals().bits()
    }

    /// Adds seals monotonically to this shmem object.
    ///
    /// Memfd seals can only be added. Once `F_SEAL_SEAL` is present,
    /// adding any new seal is rejected.
    fn add_seals(&self, new_seals: ShmemSealSet) -> VfsResult<()> {
        if new_seals.is_empty() {
            return Ok(());
        }

        let mut seals = self.seals.lock();
        if seals.contains(ShmemSealSet::SEAL) {
            return Err(kvfs::VfsError::OperationNotPermitted);
        }
        if new_seals.contains(ShmemSealSet::WRITE)
            && !seals.contains(ShmemSealSet::WRITE)
            && *self.writable_shared_pages.lock() != 0
        {
            return Err(kvfs::VfsError::ResourceBusy);
        }
        seals.insert(new_seals);
        Ok(())
    }

    /// Checks whether ordinary write operations are allowed.
    fn check_write_allowed(&self) -> VfsResult<()> {
        if self
            .seals()
            .intersects(ShmemSealSet::WRITE | ShmemSealSet::FUTURE_WRITE)
        {
            return Err(kvfs::VfsError::OperationNotPermitted);
        }
        Ok(())
    }

    /// Checks whether resizing from `old_len` to `new_len` is allowed.
    fn check_resize_allowed(&self, old_len: u64, new_len: u64) -> VfsResult<()> {
        let seals = self.seals();
        if new_len < old_len && seals.contains(ShmemSealSet::SHRINK) {
            return Err(kvfs::VfsError::OperationNotPermitted);
        }
        if new_len > old_len && seals.contains(ShmemSealSet::GROW) {
            return Err(kvfs::VfsError::OperationNotPermitted);
        }
        Ok(())
    }

    /// Checks whether a new writable shared mapping is allowed.
    fn check_shared_writable_mapping_allowed(&self) -> VfsResult<()> {
        let seals = self.seals();
        if seals.intersects(ShmemSealSet::WRITE | ShmemSealSet::FUTURE_WRITE) {
            return Err(kvfs::VfsError::OperationNotPermitted);
        }
        Ok(())
    }

    /// Checks whether an existing shared mapping may take a write fault.
    fn check_shared_write_fault_allowed(&self) -> VfsResult<()> {
        if self.seals().contains(ShmemSealSet::WRITE) {
            return Err(kvfs::VfsError::OperationNotPermitted);
        }
        Ok(())
    }

    /// Registers active `MAP_SHARED` pages.
    fn register_shared_pages(&self, pages: usize) -> VfsResult<()> {
        if pages == 0 {
            return Ok(());
        }
        let _seals = self.seals.lock();
        let mut shared_pages = self.shared_pages.lock();
        *shared_pages = shared_pages
            .checked_add(pages)
            .ok_or(kvfs::VfsError::InvalidInput)?;
        Ok(())
    }

    /// Unregisters active `MAP_SHARED` pages.
    fn unregister_shared_pages(&self, pages: usize) {
        if pages == 0 {
            return;
        }
        let mut shared_pages = self.shared_pages.lock();
        debug_assert!(*shared_pages >= pages);
        *shared_pages = shared_pages.saturating_sub(pages);
    }

    /// Registers active writable `MAP_SHARED` pages for `F_SEAL_WRITE`
    /// exclusion.
    fn register_writable_shared_pages(&self, pages: usize) -> VfsResult<()> {
        if pages == 0 {
            return Ok(());
        }
        let seals = self.seals.lock();
        if seals.intersects(ShmemSealSet::WRITE | ShmemSealSet::FUTURE_WRITE) {
            return Err(kvfs::VfsError::OperationNotPermitted);
        }
        let mut writable_shared_pages = self.writable_shared_pages.lock();
        *writable_shared_pages = writable_shared_pages
            .checked_add(pages)
            .ok_or(kvfs::VfsError::InvalidInput)?;
        drop(writable_shared_pages);
        drop(seals);
        Ok(())
    }

    /// Unregisters active writable `MAP_SHARED` pages.
    fn unregister_writable_shared_pages(&self, pages: usize) {
        if pages == 0 {
            return;
        }
        let mut writable_shared_pages = self.writable_shared_pages.lock();
        debug_assert!(*writable_shared_pages >= pages);
        *writable_shared_pages = writable_shared_pages.saturating_sub(pages);
    }

    #[cfg(unittest)]
    fn writable_shared_pages(&self) -> usize {
        *self.writable_shared_pages.lock()
    }

    #[cfg(unittest)]
    fn shared_pages(&self) -> usize {
        *self.shared_pages.lock()
    }
}

/// Anonymous tmpfs/shmem regular-file inode plus its inode-scoped policy state.
pub struct ShmemObject {
    location: Path,
    _state: Arc<ShmemObjectState>,
}

impl ShmemObject {
    /// Returns the inode location for callers that need to keep object
    /// metadata alongside the file.
    pub fn location(&self) -> &Path {
        &self.location
    }

    /// Returns the created anonymous regular-file location.
    pub fn into_path(self) -> Path {
        self.location
    }

    /// Opens this anonymous regular file and consumes the wrapper object.
    pub fn into_file(self) -> VfsResult<Arc<VfsFile>> {
        dentry_open(self.location, O_RDWR)
    }
}

/// Creates an anonymous tmpfs-backed file object.
///
/// The returned object is not inserted into a process-visible pathname
/// namespace. Its contents are owned by the normal inode-scoped page-cache
/// mapping used by KFS regular files.
fn create_anonymous_file(
    kind: ShmemObjectKind,
    name: &str,
    permission: NodePermission,
    initial_seals: ShmemSealSet,
) -> VfsResult<ShmemObject> {
    let fs = new_tmpfs(kvfs::StatFsFlags::empty());
    let root = Path::new(Mount::new_root(&fs), fs.root_dir());
    let file =
        kvfs::Filename::new(name).open_with_flags_at(&root, &root, O_CREAT | O_EXCL, permission)?;
    let location = file.path().clone();
    let state = attach_state(&location, kind, name, initial_seals)?;
    Ok(ShmemObject {
        location,
        _state: state,
    })
}

/// Creates a `memfd_create` file object.
///
/// When sealing is not allowed, the object is initialized with the
/// default `F_SEAL_SEAL`.
pub fn create_memfd_file(name: &str, allow_sealing: bool) -> VfsResult<ShmemObject> {
    let initial_seals = if allow_sealing {
        ShmemSealSet::empty()
    } else {
        ShmemSealSet::SEAL
    };
    create_anonymous_file(
        ShmemObjectKind::Memfd,
        name,
        NodePermission::from_bits_truncate(0o600),
        initial_seals,
    )
}

/// Creates a kernel-owned shmem file object for SysV shm-style users.
pub fn create_kernel_file(name: &str, permission: NodePermission) -> VfsResult<ShmemObject> {
    create_anonymous_file(
        ShmemObjectKind::Kernel,
        name,
        permission,
        ShmemSealSet::empty(),
    )
}

/// Returns inode-scoped shmem state attached to a location, if any.
fn state_for_location(location: &Path) -> Option<Arc<ShmemObjectState>> {
    location
        .downcast_node::<MemoryNode>()
        .ok()?
        .inode
        .shmem_state()
}

/// Returns memfd seal bits for a shmem location.
pub fn seal_bits_for_location(location: &Path) -> VfsResult<u32> {
    state_for_location(location)
        .map(|state| state.seal_bits())
        .ok_or(kvfs::VfsError::InvalidInput)
}

/// Adds memfd seals to a shmem location.
pub fn add_seals_for_location(location: &Path, seal_bits: u32) -> VfsResult<()> {
    let new_seals = ShmemSealSet::from_bits(seal_bits).ok_or(kvfs::VfsError::InvalidInput)?;
    let state = state_for_location(location).ok_or(kvfs::VfsError::InvalidInput)?;
    state.add_seals(new_seals)
}

/// Checks shmem write policy for a location.
pub fn check_write_allowed(location: &Path) -> VfsResult<()> {
    if let Some(state) = state_for_location(location) {
        state.check_write_allowed()?;
    }
    Ok(())
}

/// Checks shmem resize policy for a location.
pub fn check_resize_allowed(location: &Path, old_len: u64, new_len: u64) -> VfsResult<()> {
    if let Some(state) = state_for_location(location) {
        state.check_resize_allowed(old_len, new_len)?;
    }
    Ok(())
}

/// Checks shmem policy before creating or upgrading a writable shared mapping.
pub fn check_shared_writable_mapping_allowed(location: &Path) -> VfsResult<()> {
    if let Some(state) = state_for_location(location) {
        state.check_shared_writable_mapping_allowed()?;
    }
    Ok(())
}

/// Checks shmem policy before satisfying a shared write fault.
pub fn check_shared_write_fault_allowed(location: &Path) -> VfsResult<()> {
    if let Some(state) = state_for_location(location) {
        state.check_shared_write_fault_allowed()?;
    }
    Ok(())
}

/// Registers active shared pages for a shmem location.
pub fn register_shared_pages(location: &Path, pages: usize) -> VfsResult<()> {
    if let Some(state) = state_for_location(location) {
        state.register_shared_pages(pages)?;
    }
    Ok(())
}

/// Unregisters active shared pages for a shmem location.
pub fn unregister_shared_pages(location: &Path, pages: usize) {
    if let Some(state) = state_for_location(location) {
        state.unregister_shared_pages(pages);
    }
}

/// Registers active writable shared pages for a shmem location.
pub fn register_writable_shared_pages(location: &Path, pages: usize) -> VfsResult<()> {
    if let Some(state) = state_for_location(location) {
        state.register_writable_shared_pages(pages)?;
    }
    Ok(())
}

/// Unregisters active writable shared pages for a shmem location.
pub fn unregister_writable_shared_pages(location: &Path, pages: usize) {
    if let Some(state) = state_for_location(location) {
        state.unregister_writable_shared_pages(pages);
    }
}

fn attach_state(
    location: &Path,
    kind: ShmemObjectKind,
    name: &str,
    initial_seals: ShmemSealSet,
) -> VfsResult<Arc<ShmemObjectState>> {
    let node = location.downcast_node::<MemoryNode>()?;
    let state = Arc::new(ShmemObjectState::new(
        kind,
        String::from(name),
        initial_seals,
    ));
    Ok(node.inode.attach_shmem_state(state))
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert_eq, def_test};

    use super::*;

    #[def_test]
    fn shmem_write_seal_rejects_write_policy() {
        let state = ShmemObjectState::new(
            ShmemObjectKind::Memfd,
            String::from("memfd:test"),
            ShmemSealSet::WRITE | ShmemSealSet::FUTURE_WRITE,
        );

        assert_eq!(
            state.check_write_allowed(),
            Err(kvfs::VfsError::OperationNotPermitted)
        );
    }

    #[def_test]
    fn shmem_grow_and_shrink_seals_reject_matching_resize() {
        let state = ShmemObjectState::new(
            ShmemObjectKind::Memfd,
            String::from("memfd:test"),
            ShmemSealSet::GROW | ShmemSealSet::SHRINK,
        );

        assert_eq!(
            state.check_resize_allowed(4096, 8192),
            Err(kvfs::VfsError::OperationNotPermitted)
        );
        assert_eq!(
            state.check_resize_allowed(4096, 0),
            Err(kvfs::VfsError::OperationNotPermitted)
        );
        assert_eq!(state.check_resize_allowed(4096, 4096), Ok(()));
    }

    #[def_test]
    fn shmem_write_and_future_write_seals_reject_shared_writable_mapping() {
        let state = ShmemObjectState::new(
            ShmemObjectKind::Memfd,
            String::from("memfd:test"),
            ShmemSealSet::WRITE | ShmemSealSet::FUTURE_WRITE,
        );

        assert_eq!(
            state.check_shared_writable_mapping_allowed(),
            Err(kvfs::VfsError::OperationNotPermitted)
        );
        assert_eq!(
            state.check_shared_write_fault_allowed(),
            Err(kvfs::VfsError::OperationNotPermitted)
        );
    }

    #[def_test]
    fn shmem_future_write_seal_allows_existing_shared_write_fault() {
        let state = ShmemObjectState::new(
            ShmemObjectKind::Memfd,
            String::from("memfd:test"),
            ShmemSealSet::FUTURE_WRITE,
        );

        assert_eq!(
            state.check_shared_writable_mapping_allowed(),
            Err(kvfs::VfsError::OperationNotPermitted)
        );
        assert_eq!(state.check_shared_write_fault_allowed(), Ok(()));
    }

    #[def_test]
    fn shmem_add_seals_is_monotonic() {
        let state = ShmemObjectState::new(
            ShmemObjectKind::Memfd,
            String::from("memfd:test"),
            ShmemSealSet::empty(),
        );

        state.add_seals(ShmemSealSet::GROW).unwrap();
        state.add_seals(ShmemSealSet::SHRINK).unwrap();

        assert!(state.seals().contains(ShmemSealSet::GROW));
        assert!(state.seals().contains(ShmemSealSet::SHRINK));
    }

    #[def_test]
    fn shmem_seal_seal_blocks_additions() {
        let state = ShmemObjectState::new(
            ShmemObjectKind::Memfd,
            String::from("memfd:test"),
            ShmemSealSet::SEAL,
        );

        assert_eq!(
            state.add_seals(ShmemSealSet::GROW),
            Err(kvfs::VfsError::OperationNotPermitted)
        );
        assert_eq!(state.seals(), ShmemSealSet::SEAL);
    }

    #[def_test]
    fn shmem_write_seal_rejects_active_writable_shared_pages() {
        let state = ShmemObjectState::new(
            ShmemObjectKind::Memfd,
            String::from("memfd:test"),
            ShmemSealSet::empty(),
        );

        state.register_shared_pages(1).unwrap();
        state.register_writable_shared_pages(1).unwrap();
        assert_eq!(state.shared_pages(), 1);
        assert_eq!(state.writable_shared_pages(), 1);
        assert_eq!(
            state.add_seals(ShmemSealSet::WRITE),
            Err(kvfs::VfsError::ResourceBusy)
        );
        state.unregister_writable_shared_pages(1);
        state.unregister_shared_pages(1);
        assert_eq!(state.shared_pages(), 0);
        assert_eq!(state.writable_shared_pages(), 0);
        assert_eq!(state.add_seals(ShmemSealSet::WRITE), Ok(()));
    }

    #[def_test]
    fn shmem_write_seal_allows_active_readonly_shared_pages() {
        let state = ShmemObjectState::new(
            ShmemObjectKind::Memfd,
            String::from("memfd:test"),
            ShmemSealSet::empty(),
        );

        state.register_shared_pages(1).unwrap();
        assert_eq!(state.add_seals(ShmemSealSet::WRITE), Ok(()));
        assert_eq!(state.shared_pages(), 1);

        state.unregister_shared_pages(1);
        assert!(state.seals().contains(ShmemSealSet::WRITE));
    }

    #[def_test]
    fn memfd_write_seal_blocks_write_policy() {
        let shmem = create_memfd_file("memfd:test", true).unwrap();
        let location = shmem.location();

        assert_eq!(check_write_allowed(location), Ok(()));
        add_seals_for_location(location, ShmemSealSet::WRITE.bits()).unwrap();

        assert_eq!(
            check_write_allowed(location),
            Err(kvfs::VfsError::OperationNotPermitted)
        );
    }

    #[def_test]
    fn memfd_resize_seals_block_growth_and_shrink_policy() {
        let shmem = create_memfd_file("memfd:test", true).unwrap();
        let location = shmem.location();

        assert_eq!(
            add_seals_for_location(location, (ShmemSealSet::GROW | ShmemSealSet::SHRINK).bits()),
            Ok(())
        );
        assert_eq!(
            check_resize_allowed(location, 4096, 8192),
            Err(kvfs::VfsError::OperationNotPermitted)
        );
        assert_eq!(
            check_resize_allowed(location, 4096, 0),
            Err(kvfs::VfsError::OperationNotPermitted)
        );
        assert_eq!(check_resize_allowed(location, 4096, 4096), Ok(()));
    }
}

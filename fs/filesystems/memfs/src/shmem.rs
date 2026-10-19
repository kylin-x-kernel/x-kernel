// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! tmpfs/shmem-style anonymous regular-file helpers.

use alloc::{string::String, sync::Arc};

#[cfg(unittest)]
use kalloc::global_allocator;
use kcred::Cred;
use ksync::{Mutex, static_lock};
use kvfs::{
    FileSystemType, Mount, NodePermission, Path, SuperBlock, SuperBlockFlags, VfsError, VfsFile,
    VfsResult, dentry_open,
};
use linux_raw_sys::general::{
    F_SEAL_FUTURE_WRITE, F_SEAL_GROW, F_SEAL_SEAL, F_SEAL_SHRINK, F_SEAL_WRITE, O_CREAT, O_EXCL,
    O_RDWR,
};

use crate::{MemoryFs, MemoryNode, TMPFS_MAGIC, TMPFS_TYPE, quota::PageQuota};

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

/// Creates a test-only tmpfs superblock with the Linux-style default size.
///
/// Production mounts construct their superblock from parsed mount context.
/// The default is one half of the allocatable physical memory. Like Linux,
/// this is a per-superblock quota rather than a reservation.
#[cfg(unittest)]
pub(crate) fn new_tmpfs(superblock_flags: SuperBlockFlags) -> Arc<SuperBlock> {
    let page_limit = default_tmpfs_page_limit(global_allocator().total_pages());
    new_tmpfs_with_type_and_page_limit(&TMPFS_TYPE, superblock_flags, Some(page_limit))
}

pub(crate) fn new_anonymous_tmpfs(superblock_flags: SuperBlockFlags) -> Arc<SuperBlock> {
    new_tmpfs_with_type_and_page_limit(&TMPFS_TYPE, superblock_flags, None)
}

pub(crate) fn new_tmpfs_with_type_and_page_limit(
    file_system_type: &'static FileSystemType,
    superblock_flags: SuperBlockFlags,
    page_limit: Option<usize>,
) -> Arc<SuperBlock> {
    new_tmpfs_with_type_and_mount_options(
        file_system_type,
        superblock_flags,
        TmpfsMountOptions {
            page_limit,
            root_mode: default_tmpfs_root_mode(),
            root_uid: 0,
            root_gid: 0,
        },
    )
}

/// Creates a tmpfs superblock from validated mount options.
pub(crate) fn new_tmpfs_with_type_and_mount_options(
    file_system_type: &'static FileSystemType,
    superblock_flags: SuperBlockFlags,
    mount_options: TmpfsMountOptions,
) -> Arc<SuperBlock> {
    MemoryFs::new_with_name_superblock_flags_root_metadata_and_page_quota(
        file_system_type,
        TMPFS_MAGIC,
        superblock_flags,
        mount_options.root_mode,
        mount_options.root_uid,
        mount_options.root_gid,
        Some(Arc::new(PageQuota::new(mount_options.page_limit))),
    )
}

/// Returns the default Linux tmpfs page limit for a machine.
fn default_tmpfs_page_limit(total_pages: usize) -> usize {
    total_pages / 2
}

fn default_tmpfs_root_mode() -> NodePermission {
    NodePermission::from_bits_truncate(0o1777)
}

/// A page limit explicitly supplied in a tmpfs mount transaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RequestedPageLimit {
    Unlimited,
    Pages(usize),
}

/// Validated tmpfs parameters used to construct one superblock.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TmpfsMountOptions {
    page_limit: Option<usize>,
    root_mode: NodePermission,
    root_uid: u32,
    root_gid: u32,
}

/// Parsed tmpfs context state.
///
/// `None` in `requested_page_limit` means the transaction did not request a new
/// size. A new mount then uses `default_page_limit`; a remount leaves the
/// current limit unchanged.
#[derive(Clone, Copy, Debug)]
pub(super) struct TmpfsContextOptions {
    default_page_limit: usize,
    requested_page_limit: Option<RequestedPageLimit>,
    root_mode: NodePermission,
    root_uid: u32,
    root_gid: u32,
}

impl TmpfsContextOptions {
    /// Returns the limit to install for a new mount.
    pub(super) fn mount_page_limit(&self) -> Option<usize> {
        match self.requested_page_limit {
            Some(RequestedPageLimit::Pages(pages)) => Some(pages),
            Some(RequestedPageLimit::Unlimited) => None,
            None => Some(self.default_page_limit),
        }
    }

    /// Returns the parameters used to construct a new tmpfs superblock.
    pub(super) fn mount_options(&self) -> TmpfsMountOptions {
        TmpfsMountOptions {
            page_limit: self.mount_page_limit(),
            root_mode: self.root_mode,
            root_uid: self.root_uid,
            root_gid: self.root_gid,
        }
    }

    /// Returns `Some(new_limit)` only when a remount requested a size change.
    pub(super) fn reconfigure_page_limit(&self) -> Option<Option<usize>> {
        match self.requested_page_limit {
            Some(RequestedPageLimit::Pages(pages)) => Some(Some(pages)),
            Some(RequestedPageLimit::Unlimited) => Some(None),
            None => None,
        }
    }
}

/// Parses tmpfs mount options used by this implementation.
///
/// `mode`, `uid`, and `gid` select the root inode's metadata. `nr_inodes` is
/// accepts zero (unlimited); finite inode limits return `Unsupported`.
pub(super) fn parse_tmpfs_context_options(
    data: Option<&[u8]>,
    total_pages: usize,
) -> VfsResult<TmpfsContextOptions> {
    let mut options = TmpfsContextOptions {
        default_page_limit: default_tmpfs_page_limit(total_pages),
        requested_page_limit: None,
        root_mode: default_tmpfs_root_mode(),
        root_uid: 0,
        root_gid: 0,
    };

    let data = data.unwrap_or_default();
    let text = &data[..data
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(data.len())];
    for option in text.split(|byte| *byte == b',') {
        if option.is_empty() {
            continue;
        }

        if let Some(value) = option.strip_prefix(b"size=") {
            options.requested_page_limit = Some(parse_size_page_limit(value, total_pages)?);
        } else if let Some(value) = option.strip_prefix(b"nr_blocks=") {
            options.requested_page_limit = Some(parse_block_page_limit(value)?);
        } else if let Some(value) = option.strip_prefix(b"mode=") {
            options.root_mode = parse_root_mode(value)?;
        } else if let Some(value) = option.strip_prefix(b"uid=") {
            options.root_uid = parse_id(value)?;
        } else if let Some(value) = option.strip_prefix(b"gid=") {
            options.root_gid = parse_id(value)?;
        } else if let Some(value) = option.strip_prefix(b"nr_inodes=") {
            if parse_inode_limit(value)? != 0 {
                return Err(VfsError::Unsupported);
            }
        } else if option == b"noswap" {
            // X-Kernel tmpfs has no swap backend, so Linux's `noswap` policy is
            // already in effect. Accept the flag without adding a second policy bit.
        } else {
            return Err(VfsError::InvalidInput);
        }
    }
    Ok(options)
}

fn parse_size_page_limit(value: &[u8], total_pages: usize) -> VfsResult<RequestedPageLimit> {
    let (number, suffix) = split_number_and_suffix(value)?;
    let number = parse_decimal_u64(number)?;
    let multiplier = if suffix == b"%" {
        1
    } else {
        parse_suffix_multiplier(suffix)?
    };
    if number == 0 {
        return Ok(RequestedPageLimit::Unlimited);
    }

    if suffix == b"%" {
        let total_pages = u64::try_from(total_pages).map_err(|_| VfsError::InvalidInput)?;
        let pages = total_pages
            .checked_mul(number)
            .map(|pages| pages.div_ceil(100))
            .ok_or(VfsError::InvalidInput)?;
        return requested_pages(pages);
    }

    let bytes = number
        .checked_mul(multiplier)
        .ok_or(VfsError::InvalidInput)?;
    let pages = bytes.div_ceil(memaddr::PAGE_SIZE_4K as u64);
    requested_pages(pages)
}

fn parse_block_page_limit(value: &[u8]) -> VfsResult<RequestedPageLimit> {
    let (number, suffix) = split_number_and_suffix(value)?;
    let number = parse_decimal_u64(number)?;
    let multiplier = parse_suffix_multiplier(suffix)?;
    if number == 0 {
        return Ok(RequestedPageLimit::Unlimited);
    }

    let pages = number
        .checked_mul(multiplier)
        .ok_or(VfsError::InvalidInput)?;
    requested_pages(pages)
}

fn parse_inode_limit(value: &[u8]) -> VfsResult<u64> {
    let (number, suffix) = split_number_and_suffix(value)?;
    let number = parse_decimal_u64(number)?;
    let multiplier = parse_suffix_multiplier(suffix)?;
    number.checked_mul(multiplier).ok_or(VfsError::InvalidInput)
}

fn parse_root_mode(value: &[u8]) -> VfsResult<NodePermission> {
    if value.is_empty() {
        return Err(VfsError::InvalidInput);
    }
    let mode = parse_octal_u32(value)?;
    Ok(NodePermission::from_bits_truncate(
        u16::try_from(mode & 0o7777).map_err(|_| VfsError::InvalidInput)?,
    ))
}

fn parse_id(value: &[u8]) -> VfsResult<u32> {
    let id = parse_decimal_u64(value)?;
    u32::try_from(id).map_err(|_| VfsError::InvalidInput)
}

fn parse_suffix_multiplier(suffix: &[u8]) -> VfsResult<u64> {
    match suffix {
        b"" => Ok(1),
        b"k" | b"K" => Ok(1024),
        b"m" | b"M" => Ok(1024 * 1024),
        b"g" | b"G" => Ok(1024 * 1024 * 1024),
        b"t" | b"T" => Ok(1024_u64 * 1024 * 1024 * 1024),
        _ => Err(VfsError::InvalidInput),
    }
}

fn split_number_and_suffix(value: &[u8]) -> VfsResult<(&[u8], &[u8])> {
    let split_at = value
        .iter()
        .position(|byte| !byte.is_ascii_digit())
        .unwrap_or(value.len());
    let number = &value[..split_at];
    if number.is_empty() {
        return Err(VfsError::InvalidInput);
    }
    Ok((number, &value[split_at..]))
}

fn parse_decimal_u64(value: &[u8]) -> VfsResult<u64> {
    core::str::from_utf8(value)
        .map_err(|_| VfsError::InvalidInput)?
        .parse::<u64>()
        .map_err(|_| VfsError::InvalidInput)
}

fn parse_octal_u32(value: &[u8]) -> VfsResult<u32> {
    let text = core::str::from_utf8(value).map_err(|_| VfsError::InvalidInput)?;
    let mut number = 0u32;
    for byte in text.bytes() {
        let digit = u32::from(byte)
            .checked_sub(u32::from(b'0'))
            .filter(|digit| *digit < 8)
            .ok_or(VfsError::InvalidInput)?;
        number = number
            .checked_mul(8)
            .and_then(|number| number.checked_add(digit))
            .ok_or(VfsError::InvalidInput)?;
    }
    Ok(number)
}

fn requested_pages(pages: u64) -> VfsResult<RequestedPageLimit> {
    let pages = usize::try_from(pages).map_err(|_| VfsError::InvalidInput)?;
    Ok(RequestedPageLimit::Pages(pages))
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
    pub fn into_file(self, cred: Arc<Cred>) -> VfsResult<Arc<VfsFile>> {
        dentry_open(self.location, O_RDWR, cred)
    }
}

static_lock! {
    static KERNEL_SHM_FS: Mutex<Option<(Arc<SuperBlock>, Path)>> = Mutex::new(None);
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
    cred: Arc<Cred>,
) -> VfsResult<ShmemObject> {
    // Kernel-owned shm files share a single tmpfs instance to avoid
    // leaking Mount/SuperBlock references on each create/destroy cycle.
    // Memfds keep a per-instance tmpfs: their names are user-controlled and
    // a shared root would both collide same-name creates and accumulate
    // dentries nothing evicts.
    let root = match kind {
        ShmemObjectKind::Kernel => {
            let mut guard = KERNEL_SHM_FS.lock();
            guard
                .get_or_insert_with(|| {
                    let fs = new_anonymous_tmpfs(SuperBlockFlags::empty());
                    let root = Path::new(Mount::new_root(&fs), fs.root_dir());
                    (fs, root)
                })
                .1
                .clone()
        }
        ShmemObjectKind::Memfd => {
            let fs = new_anonymous_tmpfs(SuperBlockFlags::empty());
            Path::new(Mount::new_root(&fs), fs.root_dir())
        }
    };
    let file = kvfs::Filename::new(name).open_with_flags_at(
        &root,
        &root,
        O_CREAT | O_EXCL,
        permission,
        NodePermission::empty(),
        cred,
    )?;
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
pub fn create_memfd_file(
    name: &str,
    allow_sealing: bool,
    cred: Arc<Cred>,
) -> VfsResult<ShmemObject> {
    let initial_seals = if allow_sealing {
        ShmemSealSet::empty()
    } else {
        ShmemSealSet::SEAL
    };
    // Linux creates memfd files with a fixed mode 0777 (`S_IFREG |
    // S_IRWXUGO`), not subject to umask. An
    // executable mode matters for `fexecve(memfd)` / `execveat(...,
    // AT_EMPTY_PATH)`: runtimes (runc, crun) copy their own image into a
    // memfd and re-execute it for CVE-2019-5736 hardening, which requires the
    // memfd inode to permit execution.
    create_anonymous_file(
        ShmemObjectKind::Memfd,
        name,
        NodePermission::from_bits_truncate(0o777),
        initial_seals,
        cred,
    )
}

/// Creates a kernel-owned shmem file object for SysV shm-style users.
pub fn create_kernel_file(
    name: &str,
    permission: NodePermission,
    cred: Arc<Cred>,
) -> VfsResult<ShmemObject> {
    create_anonymous_file(
        ShmemObjectKind::Kernel,
        name,
        permission,
        ShmemSealSet::empty(),
        cred,
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

unittest::path_mod_test!(tests, "../ut-tests/shmem.rs");

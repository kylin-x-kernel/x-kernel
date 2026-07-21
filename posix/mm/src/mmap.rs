// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use filemap::{FileMmapRequest, mmap_private_file, mmap_shared_file};
use kerrno::{KError, KResult};
use khal::paging::{MappingFlags, PageSize};
use kprocess::current_user_process;
use kvfs::{FMode, VfsFile};
use linux_raw_sys::general::*;
use memaddr::{MemoryAddr, VirtAddr, align_up_4k};
use memspace::{AddrPolicy, MsyncPolicy, VmRuntimeRef};

bitflags::bitflags! {
    /// `PROT_*` flags for use with [`sys_mmap`].
    ///
    /// For `PROT_NONE`, use `ProtFlags::empty()`.
    #[derive(Debug, Clone, Copy)]
    struct MmapProt: u32 {
        /// Page can be read.
        const READ = PROT_READ;
        /// Page can be written.
        const WRITE = PROT_WRITE;
        /// Page can be executed.
        const EXEC = PROT_EXEC;
        /// Extend change to start of growsdown vma (mprotect only).
        const GROWDOWN = PROT_GROWSDOWN;
        /// Extend change to start of growsup vma (mprotect only).
        const GROWSUP = PROT_GROWSUP;
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct MappingPermissions {
    pub(crate) current: MappingFlags,
    pub(crate) maximum: MappingFlags,
}

impl MappingPermissions {
    fn from_prot(prot: MmapProt) -> KResult<Self> {
        if prot.intersects(MmapProt::GROWDOWN | MmapProt::GROWSUP) {
            return Err(KError::InvalidInput);
        }
        // `PROT_NONE` must translate to an empty current-permission set rather
        // than retaining `USER`, otherwise the page-table layer may keep the
        // mapping user-accessible in the current encoding.
        let mut flags = MappingFlags::empty();
        if !prot.is_empty() {
            flags |= MappingFlags::USER;
        }
        if prot.contains(MmapProt::READ) {
            flags |= MappingFlags::READ;
        }
        if prot.contains(MmapProt::WRITE) {
            flags |= MappingFlags::WRITE;
        }
        if prot.contains(MmapProt::EXEC) {
            flags |= MappingFlags::EXECUTE;
        }
        let maximum =
            MappingFlags::USER | MappingFlags::READ | MappingFlags::WRITE | MappingFlags::EXECUTE;
        Ok(Self {
            current: flags,
            maximum,
        })
    }
}

impl MmapProt {
    fn from_raw(bits: u32) -> KResult<Self> {
        Self::from_bits(bits).ok_or(KError::InvalidInput)
    }

    fn has_conflicting_grow_directions(self) -> bool {
        self.contains(Self::GROWDOWN | Self::GROWSUP)
    }
}

bitflags::bitflags! {
    /// flags for sys_mmap
    ///
    /// See <https://github.com/bminor/glibc/blob/master/bits/mman.h>
    #[derive(Debug, PartialEq, Eq, Clone, Copy)]
    struct MmapFlags: u32 {
        /// Share changes
        const SHARED = MAP_SHARED;
        /// Share changes, but fail if mapping flags contain unknown
        const SHARED_VALIDATE = MAP_SHARED_VALIDATE;
        /// Changes private; copy pages on write.
        const PRIVATE = MAP_PRIVATE;
        /// Map address must be exactly as requested, no matter whether it is available.
        const FIXED = MAP_FIXED;
        /// Same as `FIXED`, but if the requested address overlaps an existing
        /// mapping, the call fails instead of replacing the existing mapping.
        const FIXED_NOREPLACE = MAP_FIXED_NOREPLACE;
        /// Don't use a file.
        const ANONYMOUS = MAP_ANONYMOUS;
        /// Stack segment should grow down.
        const GROWSDOWN = MAP_GROWSDOWN;
        /// Legacy ignored flag.
        const EXECUTABLE = MAP_EXECUTABLE;
        /// Lock mapped pages.
        const LOCKED = MAP_LOCKED;
        /// Populate the mapping.
        const POPULATE = MAP_POPULATE;
        /// Non-blocking populate hint.
        const NONBLOCK = MAP_NONBLOCK;
        /// Don't check for reservations.
        const NORESERVE = MAP_NORESERVE;
        /// Allocation is for a stack.
        const STACK = MAP_STACK;
        /// Synchronous file-backed mapping.
        const SYNC = MAP_SYNC;
        /// Huge page
        const HUGE = MAP_HUGETLB;
        /// Huge page 2m size
        const HUGE_2MB = MAP_HUGETLB | MAP_HUGE_2MB;
        /// Huge page 1g size
        const HUGE_1GB = MAP_HUGETLB | MAP_HUGE_1GB;
        /// Deprecated flag
        const DENYWRITE = MAP_DENYWRITE;

        /// Mask for type of mapping
        const TYPE = MAP_TYPE;
    }
}

impl MremapFlags {
    fn from_raw(bits: u32) -> KResult<Self> {
        Self::from_bits(bits).ok_or(KError::InvalidInput)
    }

    fn may_move(self) -> bool {
        self.contains(Self::MAYMOVE)
    }

    fn is_fixed(self) -> bool {
        self.contains(Self::FIXED)
    }

    fn keeps_source_mapping(self) -> bool {
        self.contains(Self::DONTUNMAP)
    }

    fn requires_may_move(self) -> bool {
        self.is_fixed() || self.keeps_source_mapping()
    }

    fn validate_args(self, old_size: usize, new_size: usize) -> KResult<()> {
        if self.requires_may_move() && !self.may_move() {
            return Err(KError::InvalidInput);
        }
        if self.keeps_source_mapping() && old_size != new_size {
            return Err(KError::InvalidInput);
        }
        Ok(())
    }
}

impl MmapFlags {
    fn from_raw(bits: u32) -> KResult<Self> {
        let Some(flags) = Self::from_bits(bits) else {
            return Err(KError::InvalidInput);
        };
        Ok(flags)
    }

    fn map_type_bits(self) -> Self {
        self & Self::TYPE
    }

    fn is_anonymous(self) -> bool {
        self.contains(Self::ANONYMOUS)
    }

    fn is_hugetlb(self) -> bool {
        self.contains(Self::HUGE)
    }

    fn is_populate(self) -> bool {
        self.contains(Self::POPULATE)
    }

    fn unsupported_shared_validate_flags(self) -> Self {
        self & (Self::GROWSDOWN | Self::EXECUTABLE | Self::LOCKED | Self::NONBLOCK | Self::SYNC)
    }

    fn page_size(self) -> PageSize {
        if self.contains(Self::HUGE_1GB) {
            PageSize::Size1G
        } else if self.contains(Self::HUGE) {
            PageSize::Size2M
        } else {
            PageSize::Size4K
        }
    }

    fn addr_policy(self) -> AddrPolicy {
        if self.contains(Self::FIXED_NOREPLACE) {
            AddrPolicy::FixedNoReplace
        } else if self.contains(Self::FIXED) {
            AddrPolicy::Fixed
        } else {
            AddrPolicy::Any
        }
    }
}

/// Whether the mapping is shared or private.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MapType {
    Shared,
    Private,
}

impl MapType {
    fn from_flags(map_flags: MmapFlags) -> KResult<Self> {
        match map_flags.map_type_bits() {
            MmapFlags::PRIVATE => Ok(Self::Private),
            MmapFlags::SHARED => Ok(Self::Shared),
            MmapFlags::SHARED_VALIDATE => {
                if !map_flags.unsupported_shared_validate_flags().is_empty() {
                    return Err(KError::OperationNotSupported);
                }
                Ok(Self::Shared)
            }
            _ => Err(KError::InvalidInput),
        }
    }

    fn is_shared(self) -> bool {
        matches!(self, Self::Shared)
    }
}

fn validate_file_mapping_access(
    file: &VfsFile,
    map_type: MapType,
    current: MappingFlags,
) -> KResult {
    file.verify_mode(FMode::READ)?;
    if map_type.is_shared() && current.contains(MappingFlags::WRITE) {
        file.verify_mode(FMode::WRITE)?;
    }
    Ok(())
}

fn shared_file_max_permissions(file: &VfsFile) -> MappingFlags {
    let mut flags = MappingFlags::USER | MappingFlags::EXECUTE;
    let file_flags = file.mode();
    if file_flags.contains(FMode::READ) {
        flags |= MappingFlags::READ;
    }
    if file_flags.contains(FMode::WRITE) {
        flags |= MappingFlags::WRITE;
    }
    flags
}

pub(crate) struct MmapRequest {
    addr: usize,
    length: usize,
    fd: i32,
    offset: usize,
    flags: MmapFlags,
    map_type: MapType,
    page_size: PageSize,
    pub(crate) permissions: MappingPermissions,
}

impl MmapRequest {
    pub(crate) fn from_raw(
        addr: usize,
        length: usize,
        prot: u32,
        flags: u32,
        fd: i32,
        offset: __kernel_off_t,
    ) -> KResult<Self> {
        if length == 0 {
            return Err(KError::InvalidInput);
        }
        let offset = usize::try_from(offset).map_err(|_| KError::InvalidInput)?;
        if !PageSize::Size4K.is_aligned(offset) {
            return Err(KError::InvalidInput);
        }

        let map_flags = MmapFlags::from_raw(flags)?;
        let map_type = MapType::from_flags(map_flags)?;
        let permissions = MappingPermissions::from_prot(MmapProt::from_raw(prot)?)?;
        let page_size = map_flags.page_size();

        if map_flags.intersects(MmapFlags::FIXED | MmapFlags::FIXED_NOREPLACE)
            && !page_size.is_aligned(addr)
        {
            return Err(KError::InvalidInput);
        }
        if !map_flags.is_anonymous() && map_flags.is_hugetlb() {
            return Err(KError::InvalidInput);
        }
        if map_flags.is_anonymous() && offset != 0 {
            return Err(KError::InvalidInput);
        }

        Ok(Self {
            addr,
            length,
            fd,
            offset,
            flags: map_flags,
            map_type,
            page_size,
            permissions,
        })
    }

    pub(crate) fn resolved_page_size(&self) -> PageSize {
        if self.flags.is_anonymous() {
            self.page_size
        } else {
            PageSize::Size4K
        }
    }

    fn resolved_range(&self) -> KResult<(VirtAddr, usize, PageSize)> {
        let page_size = self.resolved_page_size();
        let start = self.addr.align_down(page_size);
        let end = self
            .addr
            .checked_add(self.length)
            .ok_or(KError::NoMemory)?
            .align_up(page_size);
        if end < start {
            return Err(KError::NoMemory);
        }
        Ok((VirtAddr::from(start), end - start, page_size))
    }
}

pub(crate) struct MunmapRequest {
    start: VirtAddr,
    length: usize,
}

impl MunmapRequest {
    pub(crate) fn from_raw(addr: usize, length: usize) -> KResult<Self> {
        if length == 0 {
            return Err(KError::InvalidInput);
        }
        Ok(Self {
            start: VirtAddr::from(addr),
            length: align_up_4k(length),
        })
    }
}

pub(crate) struct MprotectRequest {
    start: VirtAddr,
    length: usize,
    permissions: MappingPermissions,
}

impl MprotectRequest {
    pub(crate) fn from_raw(addr: usize, length: usize, prot: u32) -> KResult<Self> {
        let prot = MmapProt::from_raw(prot)?;
        if prot.has_conflicting_grow_directions() {
            return Err(KError::InvalidInput);
        }
        if prot.intersects(MmapProt::GROWDOWN | MmapProt::GROWSUP) {
            return Err(KError::OperationNotSupported);
        }
        if !addr.is_aligned_4k() {
            return Err(KError::InvalidInput);
        }
        Ok(Self {
            start: VirtAddr::from(addr),
            length: align_up_4k(length),
            permissions: MappingPermissions::from_prot(prot)?,
        })
    }
}

pub(crate) struct MadviseRequest {
    start: VirtAddr,
    length: usize,
}

impl MadviseRequest {
    pub(crate) fn dontneed_from_raw(
        addr: usize,
        length: usize,
        advice: i32,
    ) -> KResult<Option<Self>> {
        if advice != MADV_DONTNEED as i32 {
            return Err(KError::InvalidInput);
        }
        if addr & (memaddr::PAGE_SIZE_4K - 1) != 0 {
            return Err(KError::InvalidInput);
        }
        let end = addr.checked_add(length).ok_or(KError::InvalidInput)?;
        if length == 0 {
            return Ok(None);
        }

        let start = VirtAddr::from(addr);
        let end = VirtAddr::from(align_up_4k(end));
        Ok(Some(Self {
            start,
            length: end.as_usize().saturating_sub(start.as_usize()),
        }))
    }
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct MsyncFlags: u32 {
        const ASYNC = MS_ASYNC;
        const INVALIDATE = MS_INVALIDATE;
        const SYNC = MS_SYNC;
    }
}

pub(crate) struct MsyncRequest {
    start: VirtAddr,
    length: usize,
    flags: MsyncFlags,
}

impl MsyncRequest {
    pub(crate) fn from_raw(addr: usize, length: usize, flags: u32) -> KResult<Self> {
        let flags = MsyncFlags::from_bits(flags).ok_or(KError::InvalidInput)?;
        if flags.contains(MsyncFlags::ASYNC | MsyncFlags::SYNC) {
            return Err(KError::InvalidInput);
        }
        if !addr.is_aligned_4k() {
            return Err(KError::InvalidInput);
        }
        let end = addr.checked_add(length).ok_or(KError::NoMemory)?;
        let end = checked_align_up_4k(end)?;
        Ok(Self {
            start: VirtAddr::from(addr),
            length: end.checked_sub(addr).ok_or(KError::NoMemory)?,
            flags,
        })
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.length == 0
    }

    pub(crate) fn policy(&self) -> KResult<MsyncPolicy> {
        MsyncPolicy::try_new(
            self.flags.contains(MsyncFlags::SYNC),
            self.flags.contains(MsyncFlags::ASYNC),
            self.flags.contains(MsyncFlags::INVALIDATE),
            true,
        )
    }
}

fn checked_align_up_4k(value: usize) -> KResult<usize> {
    let mask = memaddr::PAGE_SIZE_4K - 1;
    value
        .checked_add(mask)
        .map(|it| it & !mask)
        .ok_or(KError::NoMemory)
}

pub fn sys_mmap(
    addr: usize,
    length: usize,
    prot: u32,
    flags: u32,
    fd: i32,
    offset: __kernel_off_t,
) -> KResult<isize> {
    let request = MmapRequest::from_raw(addr, length, prot, flags, fd, offset)?;

    let process = current_user_process();
    let file = if !request.flags.is_anonymous() {
        Some(process.resources()?.get_file(request.fd)?)
    } else {
        None
    };

    // --- Resolve the mapping address ---
    let (hint, mut length, page_size) = request.resolved_range()?;

    let aspace_ref = process.address_space()?;
    aspace_ref.with_mapping_owner(|mut mapping| {
        let start = mapping.aspace_mut().mmap_resolve_addr(
            hint,
            length,
            page_size as usize,
            request.flags.addr_policy(),
        )?;

        debug!(
            "sys_mmap <= addr: {addr:#x?}, length: {length:#x?}, permissions: {:?}, flags: {:?}, \
             fd: {:?}, offset: {:?}",
            request.permissions, request.flags, request.fd, request.offset
        );

        let file_vma = match file.as_ref() {
            None => {
                // Anonymous mapping
                None
            }
            Some(file) => {
                validate_file_mapping_access(file, request.map_type, request.permissions.current)?;
                let max_flags = if request.map_type.is_shared() {
                    shared_file_max_permissions(file)
                } else {
                    request.permissions.maximum
                };
                let invalidate = mapping.invalidate_handle();
                let req = FileMmapRequest {
                    start,
                    length,
                    offset: request.offset,
                    page_size,
                    flags: request.permissions.current,
                    max_flags,
                    file: file.clone(),
                    mm_id: mapping.aspace().mm_id(),
                    observer: mapping.observer(),
                    invalidate,
                };
                let (vma, runtime) = if request.map_type.is_shared() {
                    mmap_shared_file(req)?
                } else {
                    mmap_private_file(FileMmapRequest {
                        invalidate: req.invalidate.clone(),
                        ..req
                    })?
                };
                length = vma.size();
                Some((vma, runtime))
            }
        };

        if let Some((vma, runtime)) = file_vma {
            mapping
                .aspace_mut()
                .map_runtime_vma(vma, request.flags.is_populate(), runtime)?;
        } else {
            let runtime = match request.map_type {
                MapType::Shared => VmRuntimeRef::new_anon_shared(start, length, page_size)?,
                MapType::Private => VmRuntimeRef::new_anon_private(start, page_size),
            };
            mapping.aspace_mut().map_with_max_flags(
                start,
                length,
                request.permissions.current,
                request.permissions.maximum,
                request.flags.is_populate(),
                runtime,
            )?;
        }

        Ok(start.as_usize() as _)
    })
}

pub fn sys_munmap(addr: usize, length: usize) -> KResult<isize> {
    debug!("sys_munmap <= addr: {addr:#x}, length: {length:x}");
    let request = MunmapRequest::from_raw(addr, length)?;
    let process = current_user_process();
    let aspace_ref = process.address_space()?;
    let mut aspace = aspace_ref.lock();
    aspace.unmap(request.start, request.length)?;
    Ok(0)
}

pub fn sys_mprotect(addr: usize, length: usize, prot: u32) -> KResult<isize> {
    // TODO: implement PROT_GROWSUP & PROT_GROWSDOWN
    let request = MprotectRequest::from_raw(addr, length, prot)?;
    debug!(
        "sys_mprotect <= addr: {addr:#x}, length: {length:x}, permissions: {:?}",
        request.permissions
    );

    let process = current_user_process();
    let aspace_ref = process.address_space()?;
    let mut aspace = aspace_ref.lock();
    aspace.protect(request.start, request.length, request.permissions.current)?;

    Ok(0)
}

bitflags::bitflags! {
    /// `MREMAP_*` flags for use with [`sys_mremap`].
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct MremapFlags: u32 {
        const MAYMOVE = MREMAP_MAYMOVE;
        const FIXED = MREMAP_FIXED;
        const DONTUNMAP = MREMAP_DONTUNMAP;
    }
}

pub fn sys_mremap(
    addr: usize,
    old_size: usize,
    new_size: usize,
    flags: u32,
    new_addr: usize,
) -> KResult<isize> {
    debug!(
        "sys_mremap <= addr: {addr:#x}, old_size: {old_size:x}, new_size: {new_size:x}, flags: \
         {flags:#x}, new_addr: {new_addr:#x}"
    );

    // --- 1. Parameter validation ---

    if new_size == 0 {
        return Err(KError::InvalidInput);
    }

    let mremap_flags = MremapFlags::from_raw(flags)?;
    mremap_flags.validate_args(old_size, new_size)?;

    if !addr.is_multiple_of(PageSize::Size4K as usize) {
        return Err(KError::InvalidInput);
    }
    let old_size = align_up_4k(old_size);
    let new_size = align_up_4k(new_size);

    if mremap_flags.is_fixed() {
        if !new_addr.is_multiple_of(PageSize::Size4K as usize) {
            return Err(KError::InvalidInput);
        }
        let new_end = new_addr.checked_add(new_size).ok_or(KError::InvalidInput)?;
        let old_end = addr.checked_add(old_size).ok_or(KError::InvalidInput)?;
        if new_addr < old_end && addr < new_end {
            return Err(KError::InvalidInput);
        }
    }

    let addr = VirtAddr::from(addr);

    // --- 2. Find the VMA ---

    let process = current_user_process();
    let aspace_ref = process.address_space()?;
    aspace_ref.with_mapping_owner(|mut mapping| {
        let source = mapping.aspace_mut().resolve_mremap_source(addr, old_size)?;
        let vma_end = source.end();
        let mapping_flags = source.flags();
        let max_flags = source.max_flags();
        let page_size = source.page_size();

        // --- 3. Dispatch ---

        // FIXED path: move to new_addr (handles both shrink and grow)
        if mremap_flags.is_fixed() {
            // If shrinking, trim source first
            if new_size < old_size {
                mapping
                    .aspace_mut()
                    .unmap(addr + new_size, old_size - new_size)?;
            }
            let move_size = old_size.min(new_size);
            // Unmap target region
            mapping
                .aspace_mut()
                .unmap(VirtAddr::from(new_addr), new_size)?;
            let target_addr = VirtAddr::from(new_addr);
            mapping.map_relocated_snapshot(&source, target_addr, new_size, mapping_flags)?;
            mapping
                .aspace_mut()
                .move_pages(addr, target_addr, move_size, page_size)?;
            mapping
                .aspace_mut()
                .drop_mapping_metadata(addr, move_size)?;
            if mremap_flags.keeps_source_mapping() {
                let fresh_runtime = VmRuntimeRef::new_anon_private(addr, page_size);
                mapping.aspace_mut().map_with_max_flags(
                    addr,
                    move_size,
                    mapping_flags,
                    max_flags,
                    false,
                    fresh_runtime,
                )?;
            }
            return Ok(target_addr.as_usize() as _);
        }

        // DONTUNMAP path: move, leave old as fresh anonymous
        if mremap_flags.keeps_source_mapping() {
            let target_addr = mapping
                .aspace_mut()
                .find_relocation_target(addr, new_size, page_size)?;

            mapping.map_relocated_snapshot(&source, target_addr, new_size, mapping_flags)?;
            mapping
                .aspace_mut()
                .move_pages(addr, target_addr, old_size, page_size)?;
            // Old mapping: retire the moved source role, then install a fresh
            // anonymous mapping at the original address.
            mapping.aspace_mut().drop_mapping_metadata(addr, old_size)?;
            let fresh_runtime = VmRuntimeRef::new_anon_private(addr, page_size);
            mapping.aspace_mut().map_with_max_flags(
                addr,
                old_size,
                mapping_flags,
                max_flags,
                false,
                fresh_runtime,
            )?;
            return Ok(target_addr.as_usize() as _);
        }

        // No-op: same size, no FIXED, no DONTUNMAP
        if new_size == old_size {
            return Ok(addr.as_usize() as _);
        }

        // Shrink (no FIXED, no DONTUNMAP)
        if new_size < old_size {
            mapping
                .aspace_mut()
                .unmap(addr + new_size, old_size - new_size)?;
            return Ok(addr.as_usize() as _);
        }

        // --- 4. Grow (new_size > old_size, no FIXED, no DONTUNMAP) ---

        // Can only grow in place if old_size covers the entire VMA
        let can_grow_in_place = addr.as_usize() + old_size == vma_end.as_usize();

        if can_grow_in_place {
            match mapping.aspace_mut().extend_area(addr, new_size - old_size) {
                Ok(()) => return Ok(addr.as_usize() as _),
                Err(e) => debug!("in-place grow failed ({e:?}), falling back to move"),
            }
        }

        // In-place grow failed or not possible — fall back to move if MAYMOVE
        if !mremap_flags.may_move() {
            return Err(KError::NoMemory);
        }

        let target_addr = mapping
            .aspace_mut()
            .find_relocation_target(addr, new_size, page_size)?;

        let move_size = old_size.min(new_size);
        mapping.map_relocated_snapshot(&source, target_addr, new_size, mapping_flags)?;
        mapping
            .aspace_mut()
            .move_pages(addr, target_addr, move_size, page_size)?;
        mapping.aspace_mut().drop_mapping_metadata(addr, old_size)?;

        Ok(target_addr.as_usize() as _)
    })
}

pub fn sys_madvise(addr: usize, length: usize, advice: i32) -> KResult<isize> {
    debug!("sys_madvise <= addr: {addr:#x}, length: {length:x}, advice: {advice:#x}");

    let Some(request) = MadviseRequest::dontneed_from_raw(addr, length, advice)? else {
        return Ok(0);
    };

    let process = current_user_process();
    let aspace_ref = process.address_space()?;
    let mut aspace = aspace_ref.lock();
    aspace.madvise_dontneed(request.start, request.length)?;
    Ok(0)
}

pub fn sys_msync(addr: usize, length: usize, flags: u32) -> KResult<isize> {
    debug!("sys_msync <= addr: {addr:#x}, length: {length:x}, flags: {flags:#x}");

    let request = MsyncRequest::from_raw(addr, length, flags)?;
    if request.is_empty() {
        return Ok(0);
    }
    let process = current_user_process();
    let aspace_ref = process.address_space()?;
    let mut aspace = aspace_ref.lock();
    aspace.msync_range(request.start, request.length, request.policy()?)?;
    Ok(0)
}

pub fn sys_mlock(addr: usize, length: usize) -> KResult<isize> {
    sys_mlock2(addr, length, 0)
}

pub fn sys_mlock2(_addr: usize, _length: usize, _flags: u32) -> KResult<isize> {
    Ok(0)
}

#[cfg(unittest)]
mod tests {
    use linux_raw_sys::general::{MREMAP_DONTUNMAP, MREMAP_FIXED, MREMAP_MAYMOVE};
    use unittest::def_test;

    use super::MremapFlags;

    #[def_test]
    fn mremap_dontunmap_requires_equal_sizes() {
        let flags = MremapFlags::from_raw(MREMAP_MAYMOVE | MREMAP_DONTUNMAP).unwrap();

        assert!(flags.validate_args(0x2000, 0x3000).is_err());
        assert!(flags.validate_args(0x2000, 0x2000).is_ok());
    }

    #[def_test]
    fn mremap_fixed_and_dontunmap_require_maymove() {
        assert!(
            MremapFlags::from_raw(MREMAP_FIXED)
                .unwrap()
                .validate_args(0x2000, 0x2000)
                .is_err()
        );
        assert!(
            MremapFlags::from_raw(MREMAP_DONTUNMAP)
                .unwrap()
                .validate_args(0x2000, 0x2000)
                .is_err()
        );
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kerrno::{KError, KResult};
use kfd::FileLike;
use kfs::File;
use khal::paging::{MappingFlags, PageSize};
use kthread::current_process_state;
use linux_raw_sys::general::*;
use memaddr::{MemoryAddr, VirtAddr, align_up_4k};
use memspace::{
    AddrPolicy,
    backend::{Backend, BackendOps},
};
use memspace_file::{FileMapper, new_alloc};

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

impl From<MmapProt> for MappingFlags {
    fn from(value: MmapProt) -> Self {
        let mut flags = MappingFlags::USER;
        if value.contains(MmapProt::READ) {
            flags |= MappingFlags::READ;
        }
        if value.contains(MmapProt::WRITE) {
            flags |= MappingFlags::WRITE;
        }
        if value.contains(MmapProt::EXEC) {
            flags |= MappingFlags::EXECUTE;
        }
        flags
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
        /// Populate the mapping.
        const POPULATE = MAP_POPULATE;
        /// Don't check for reservations.
        const NORESERVE = MAP_NORESERVE;
        /// Allocation is for a stack.
        const STACK = MAP_STACK;
        /// Huge page
        const HUGE = MAP_HUGETLB;
        /// Huge page 1g size
        const HUGE_1GB = MAP_HUGETLB | MAP_HUGE_1GB;
        /// Deprecated flag
        const DENYWRITE = MAP_DENYWRITE;

        /// Mask for type of mapping
        const TYPE = MAP_TYPE;
    }
}

/// Whether the mapping is shared or private.
#[derive(Clone, Copy, PartialEq)]
enum MapType {
    Shared,
    Private,
}

pub fn sys_mmap(
    addr: usize,
    length: usize,
    prot: u32,
    flags: u32,
    fd: i32,
    offset: isize,
) -> KResult<isize> {
    // --- 1. Basic parameter checks ---

    if length == 0 {
        return Err(KError::InvalidInput);
    }

    let map_flags = MmapFlags::from_bits_truncate(flags);
    let map_type_flag = map_flags & MmapFlags::TYPE;
    if !matches!(
        map_type_flag,
        MmapFlags::PRIVATE | MmapFlags::SHARED | MmapFlags::SHARED_VALIDATE
    ) {
        return Err(KError::InvalidInput);
    }

    // SHARED_VALIDATE rejects unknown flags; SHARED/PRIVATE silently ignore them.
    if map_type_flag == MmapFlags::SHARED_VALIDATE && MmapFlags::from_bits(flags).is_none() {
        return Err(KError::OperationNotSupported);
    }

    let map_type = match map_type_flag {
        MmapFlags::PRIVATE => MapType::Private,
        _ => MapType::Shared,
    };

    // --- 2. Branch: file-backed vs anonymous ---

    let proc_state = current_process_state();

    let file = if map_flags.contains(MmapFlags::ANONYMOUS) {
        // Anonymous mapping: fd must be invalid, offset must be zero.
        if fd > 0 {
            return Err(KError::InvalidInput);
        }
        if offset != 0 {
            return Err(KError::InvalidInput);
        }
        None
    } else {
        // File-backed mapping: resolve fd and validate offset.
        if fd < 0 {
            return Err(KError::BadFileDescriptor);
        }
        let offset: usize = offset.try_into().map_err(|_| KError::InvalidInput)?;
        if !PageSize::Size4K.is_aligned(offset) {
            return Err(KError::InvalidInput);
        }
        let file = proc_state.resources.get_file_like_as::<File>(fd)?;
        // The file's mmap callback will validate permissions and mapping support.
        Some((file, offset))
    };

    let permission_flags = MmapProt::from_bits_truncate(prot);

    debug!(
        "sys_mmap <= addr: {addr:#x?}, length: {length:#x?}, prot: {permission_flags:?}, flags: \
         {map_flags:?}, fd: {fd:?}, offset: {offset:?}"
    );

    // --- 3. Determine page size and align ---

    let page_size = if map_flags.contains(MmapFlags::HUGE_1GB) {
        PageSize::Size1G
    } else if map_flags.contains(MmapFlags::HUGE) {
        PageSize::Size2M
    } else {
        PageSize::Size4K
    };

    let start = addr.align_down(page_size);
    let end = (addr + length).align_up(page_size);
    if end < start {
        return Err(KError::NoMemory);
    }
    let mut length = end - start;

    // --- 4. Resolve the mapping address ---

    let mut aspace = proc_state.address_space().lock();

    let policy = if map_flags.contains(MmapFlags::FIXED_NOREPLACE) {
        AddrPolicy::FixedNoReplace
    } else if map_flags.contains(MmapFlags::FIXED) {
        AddrPolicy::Fixed
    } else {
        AddrPolicy::Any
    };
    let start =
        aspace.mmap_resolve_addr(VirtAddr::from(start), length, page_size as usize, policy)?;

    // --- 5. Create backend ---

    let backend = match file {
        None => {
            // Anonymous mapping
            match map_type {
                MapType::Shared => Backend::new_anonymous_shared(start, length, PageSize::Size4K)?,
                MapType::Private => new_alloc(start, page_size),
            }
        }
        Some((file, offset)) => {
            let mut mapper = FileMapper::new(
                start,
                length,
                offset,
                page_size,
                map_type == MapType::Shared,
                file.clone(),
                proc_state.address_space().clone(),
            );
            file.mmap(&mut mapper)?;
            length = mapper.length;
            mapper.into_backend()?
        }
    };

    let populate = map_flags.contains(MmapFlags::POPULATE);
    aspace.map(start, length, permission_flags.into(), populate, backend)?;

    Ok(start.as_usize() as _)
}

pub fn sys_munmap(addr: usize, length: usize) -> KResult<isize> {
    debug!("sys_munmap <= addr: {addr:#x}, length: {length:x}");
    let proc_state = current_process_state();
    let mut aspace = proc_state.address_space().lock();
    let length = align_up_4k(length);
    let start_addr = VirtAddr::from(addr);
    aspace.unmap(start_addr, length)?;
    Ok(0)
}

pub fn sys_mprotect(addr: usize, length: usize, prot: u32) -> KResult<isize> {
    // TODO: implement PROT_GROWSUP & PROT_GROWSDOWN
    let Some(permission_flags) = MmapProt::from_bits(prot) else {
        return Err(KError::InvalidInput);
    };
    debug!("sys_mprotect <= addr: {addr:#x}, length: {length:x}, prot: {permission_flags:?}");

    if permission_flags.contains(MmapProt::GROWDOWN | MmapProt::GROWSUP) {
        return Err(KError::InvalidInput);
    }

    if !addr.is_aligned_4k() {
        return Err(KError::InvalidInput);
    }

    let proc_state = current_process_state();
    let mut aspace = proc_state.address_space().lock();
    let length = align_up_4k(length);
    let start_addr = VirtAddr::from(addr);
    aspace.protect(start_addr, length, permission_flags.into())?;

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

    let mremap_flags = MremapFlags::from_bits(flags).ok_or(KError::InvalidInput)?;

    if (mremap_flags.contains(MremapFlags::FIXED) || mremap_flags.contains(MremapFlags::DONTUNMAP))
        && !mremap_flags.contains(MremapFlags::MAYMOVE)
    {
        return Err(KError::InvalidInput);
    }

    if mremap_flags.contains(MremapFlags::DONTUNMAP) && old_size != new_size {
        return Err(KError::InvalidInput);
    }

    if !addr.is_multiple_of(PageSize::Size4K as usize) {
        return Err(KError::InvalidInput);
    }
    let old_size = align_up_4k(old_size);
    let new_size = align_up_4k(new_size);

    if mremap_flags.contains(MremapFlags::FIXED) {
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

    let proc_state = current_process_state();
    let mut aspace = proc_state.address_space().lock();

    let area = aspace.find_area(addr).ok_or(KError::BadAddress)?;
    let vma_start = area.start();
    let vma_end = area.end();
    let mapping_flags = area.flags();
    let page_size = area.backend().page_size();
    let backend = area.backend().clone();
    let _ = area; // release borrow

    if addr != vma_start {
        return Err(KError::InvalidInput);
    }

    if addr.as_usize() + old_size > vma_end.as_usize() {
        return Err(KError::BadAddress);
    }

    if !page_size.is_aligned(addr.as_usize()) {
        return Err(KError::InvalidInput);
    }

    // --- 3. Dispatch ---

    let aspace_ref = proc_state.address_space().clone();

    // FIXED path: move to new_addr (handles both shrink and grow)
    if mremap_flags.contains(MremapFlags::FIXED) {
        // If shrinking, trim source first
        if new_size < old_size {
            aspace.unmap(addr + new_size, old_size - new_size)?;
        }
        let move_size = old_size.min(new_size);
        // Unmap target region
        aspace.unmap(VirtAddr::from(new_addr), new_size)?;
        let target_addr = VirtAddr::from(new_addr);
        let relocated_backend = backend.relocated(target_addr, &aspace_ref)?;
        aspace.map(
            target_addr,
            new_size,
            mapping_flags,
            false,
            relocated_backend,
        )?;
        aspace.move_pages(addr, target_addr, move_size, page_size)?;
        aspace.unmap(addr, move_size)?;
        if mremap_flags.contains(MremapFlags::DONTUNMAP) {
            let fresh_backend = new_alloc(addr, page_size);
            aspace.map(addr, move_size, mapping_flags, false, fresh_backend)?;
        }
        return Ok(target_addr.as_usize() as _);
    }

    // DONTUNMAP path: move, leave old as fresh anonymous
    if mremap_flags.contains(MremapFlags::DONTUNMAP) {
        let limit = memaddr::VirtAddrRange::new(aspace.base(), aspace.end());
        let target_addr = aspace
            .find_free_area(addr, new_size, limit, page_size as usize)
            .or(aspace.find_free_area(aspace.base(), new_size, limit, page_size as usize))
            .ok_or(KError::NoMemory)?;

        let relocated_backend = backend.relocated(target_addr, &aspace_ref)?;
        aspace.map(
            target_addr,
            new_size,
            mapping_flags,
            false,
            relocated_backend,
        )?;
        aspace.move_pages(addr, target_addr, old_size, page_size)?;
        // Old mapping: unmap metadata + remap fresh anonymous
        aspace.unmap(addr, old_size)?;
        let fresh_backend = new_alloc(addr, page_size);
        aspace.map(addr, old_size, mapping_flags, false, fresh_backend)?;
        return Ok(target_addr.as_usize() as _);
    }

    // No-op: same size, no FIXED, no DONTUNMAP
    if new_size == old_size {
        return Ok(addr.as_usize() as _);
    }

    // Shrink (no FIXED, no DONTUNMAP)
    if new_size < old_size {
        aspace.unmap(addr + new_size, old_size - new_size)?;
        return Ok(addr.as_usize() as _);
    }

    // --- 4. Grow (new_size > old_size, no FIXED, no DONTUNMAP) ---

    // Can only grow in place if old_size covers the entire VMA
    let can_grow_in_place = addr.as_usize() + old_size == vma_end.as_usize();

    if can_grow_in_place {
        match aspace.extend_area(addr, new_size - old_size) {
            Ok(()) => return Ok(addr.as_usize() as _),
            Err(e) => debug!("in-place grow failed ({e:?}), falling back to move"),
        }
    }

    // In-place grow failed or not possible — fall back to move if MAYMOVE
    if !mremap_flags.contains(MremapFlags::MAYMOVE) {
        return Err(KError::NoMemory);
    }

    let limit = memaddr::VirtAddrRange::new(aspace.base(), aspace.end());
    let target_addr = aspace
        .find_free_area(addr, new_size, limit, page_size as usize)
        .or(aspace.find_free_area(aspace.base(), new_size, limit, page_size as usize))
        .ok_or(KError::NoMemory)?;

    let move_size = old_size.min(new_size);
    let relocated_backend = backend.relocated(target_addr, &aspace_ref)?;
    aspace.map(
        target_addr,
        new_size,
        mapping_flags,
        false,
        relocated_backend,
    )?;
    aspace.move_pages(addr, target_addr, move_size, page_size)?;
    aspace.unmap(addr, old_size)?;

    Ok(target_addr.as_usize() as _)
}

pub fn sys_madvise(addr: usize, length: usize, advice: i32) -> KResult<isize> {
    debug!("sys_madvise <= addr: {addr:#x}, length: {length:x}, advice: {advice:#x}");
    Ok(0)
}

pub fn sys_msync(addr: usize, length: usize, flags: u32) -> KResult<isize> {
    debug!("sys_msync <= addr: {addr:#x}, length: {length:x}, flags: {flags:#x}");

    Ok(0)
}

pub fn sys_mlock(addr: usize, length: usize) -> KResult<isize> {
    sys_mlock2(addr, length, 0)
}

pub fn sys_mlock2(_addr: usize, _length: usize, _flags: u32) -> KResult<isize> {
    Ok(0)
}

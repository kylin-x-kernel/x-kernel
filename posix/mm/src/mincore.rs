// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Memory resident status syscalls.

use alloc::vec;

use kerrno::{KError, KResult};
use khal::paging::{MappingFlags, PageSize, PagingError};
use kthread::current_process_state;
use memaddr::{MemoryAddr, PAGE_SIZE_4K, PhysAddr, VirtAddr};
use posix_types::UserPtr;

struct ResidencyQuery {
    is_resident: bool,
    step_bytes: usize,
}

impl ResidencyQuery {
    fn from_page_table_query(
        query_result: Result<(PhysAddr, MappingFlags, PageSize), PagingError>,
    ) -> Self {
        match query_result {
            Ok((_, _, size)) => Self {
                is_resident: true,
                step_bytes: size as usize,
            },
            Err(_) => Self {
                is_resident: false,
                step_bytes: PAGE_SIZE_4K,
            },
        }
    }

    fn step_pages(&self) -> usize {
        self.step_bytes / PAGE_SIZE_4K
    }
}

fn ensure_user_accessible_area(aspace: &memspace::AddrSpace, addr: VirtAddr) -> KResult<()> {
    let area = aspace.find_area(addr).ok_or(KError::NoMemory)?;
    if !area.flags().contains(MappingFlags::USER) {
        return Err(KError::NoMemory);
    }
    Ok(())
}

/// Check whether pages are resident in memory.
///
/// The mincore() system call determines whether pages of the calling process's
/// virtual memory are resident in RAM.
///
/// # Arguments
/// * `addr` - Starting address (must be a multiple of the page size)
/// * `length` - Length of the region in bytes (effectively rounded up to next page boundary)
/// * `vec` - Output array containing at least (length+PAGE_SIZE-1)/PAGE_SIZE bytes.
///
/// # Return Value
/// * `Ok(0)` on success
/// * `Err(EAGAIN)` - Kernel is temporarily out of resources (not implemented in Kernel)
/// * `Err(EFAULT)` - vec points to an invalid address (dispatch_irqd by write_vm_mem)
/// * `Err(EINVAL)` - addr is not a multiple of the page size
/// * `Err(ENOMEM)` - length is greater than (TASK_SIZE - addr), or negative length, or `addr` to `addr`+`length` contained unmapped memory
///
/// # Notes from Linux man page
/// - The least significant bit (bit 0) is set if page is resident in memory
/// - Bits 1-7 are reserved and currently cleared
/// - Information is only a snapshot; pages can be swapped at any moment
///
/// # Linux Errors
/// - EAGAIN:  kernel temporarily out of resources
/// - EFAULT: vec points to invalid address
/// - EINVAL: addr not page-aligned
/// - ENOMEM: length > (TASK_SIZE - addr), negative length, or unmapped memory
pub fn sys_mincore(addr: usize, length: usize, vec: UserPtr<u8>) -> KResult<isize> {
    let start_addr = VirtAddr::from(addr);

    // EINVAL: addr must be a multiple of the page size
    if !start_addr.is_aligned(PAGE_SIZE_4K) {
        return Err(KError::InvalidInput);
    }

    // EFAULT: vec must not be null (basic check, write_vm_mem will do full validation)
    if vec.is_null() {
        return Err(KError::BadAddress);
    }

    debug!("sys_mincore <= addr: {addr:#x}, length: {length:#x}");

    // Special case: length=0
    // According to Linux kernel (mm/mincore.c), length=0 returns success
    // WITHOUT validating that addr is mapped.  This is intentional behavior
    // to match POSIX semantics where a zero-length operation is a no-op.
    if length == 0 {
        return Ok(0);
    }

    // Calculate number of pages to check
    let page_count = length.div_ceil(PAGE_SIZE_4K);

    // Get current address space
    let proc_state = current_process_state();
    let aspace = proc_state.address_space().lock();

    let mut result = vec![0u8; page_count];
    let mut i = 0;

    while i < page_count {
        let addr = start_addr + i * PAGE_SIZE_4K;

        ensure_user_accessible_area(&aspace, addr)?;
        let residency = ResidencyQuery::from_page_table_query(aspace.page_table().query(addr));
        let step_pages = residency.step_pages();

        if residency.is_resident {
            let end = (i + step_pages).min(page_count);
            result[i..end].fill(1);
        }

        i += step_pages;
    }

    // EFAULT: Write result to user space
    vec.write_vm_slice(result.as_slice())?;

    Ok(0)
}

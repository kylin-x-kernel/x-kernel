// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Runtime MMIO mapping backend and device-region registry.

extern crate alloc;

use alloc::vec::Vec;

use kaddr_layout::{IOMAP_VADDR, IOMAP_VSIZE};
use khal::paging::MappingFlags;
use kspin::SpinNoIrq;
use memaddr::{MemoryAddr, PhysAddr, VirtAddr, VirtAddrRange, va};

use crate::kernel_layout;

/// Error returned by runtime device IO mapping helpers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoMapError {
    InvalidRange,
    MappingFailed,
    NoMemory,
}

/// A registered device MMIO region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceRegion {
    pub paddr: PhysAddr,
    pub size: usize,
    pub name: &'static str,
    pub vaddr: Option<VirtAddr>,
}

/// Iterator over registered device MMIO regions.
pub type DeviceRegionIter = alloc::vec::IntoIter<DeviceRegion>;

/// Lifetime policy of a registered MMIO region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegionRefs {
    /// Statically owned region (fixed/boot mapping). Never auto-unmapped.
    Pinned,
    /// Runtime `iomap_device` mapping with an active reference count.
    Counted(usize),
}

/// Internal registry entry pairing a region with its lifetime policy.
#[derive(Debug, Clone, Copy)]
struct RegionEntry {
    region: DeviceRegion,
    refs: RegionRefs,
}

static DEVICE_REGISTRY: SpinNoIrq<Vec<RegionEntry>> = SpinNoIrq::new(Vec::new());

// Boot/runtime layout ownership lives in kaddr_layout. memspace only consumes
// the dedicated I/O VA window and allocates device mappings from that range.
fn iomap_window() -> VirtAddrRange {
    VirtAddrRange::from_start_size(va!(IOMAP_VADDR), IOMAP_VSIZE)
}

/// Register a named device MMIO region for diagnostics and memory-region views.
pub fn register_device_region(
    paddr: PhysAddr,
    size: usize,
    name: &'static str,
) -> Result<(), IoMapError> {
    register_region(DeviceRegion {
        paddr,
        size,
        name,
        vaddr: None,
    })
}

/// Register a named device MMIO region that already has a fixed virtual address.
pub fn register_fixed_device_region(
    paddr: PhysAddr,
    size: usize,
    name: &'static str,
    vaddr: VirtAddr,
) -> Result<(), IoMapError> {
    register_region(DeviceRegion {
        paddr,
        size,
        name,
        vaddr: Some(vaddr),
    })
}

/// Return an iterator over runtime-registered device MMIO regions.
pub fn device_regions() -> DeviceRegionIter {
    DEVICE_REGISTRY
        .lock()
        .iter()
        .map(|entry| entry.region)
        .collect::<Vec<_>>()
        .into_iter()
}

/// Map a device MMIO region and register it for diagnostics.
///
/// Drivers are expected to call this during initialization, cache the returned
/// virtual base, and reuse that base from fast paths such as IRQ handlers.
///
/// Each successful call takes one reference on the underlying mapping. Callers
/// that want lifetime-managed teardown should pair this with [`iounmap`].
pub fn iomap_device(
    paddr: PhysAddr,
    size: usize,
    name: &'static str,
) -> Result<VirtAddr, IoMapError> {
    if size == 0 || paddr.checked_add(size).is_none() {
        return Err(IoMapError::InvalidRange);
    }

    let start = paddr.align_down_4k();
    let end = paddr
        .checked_add(size)
        .ok_or(IoMapError::InvalidRange)?
        .align_up_4k();
    let span = end.sub_addr(start);

    if let Some(vaddr) = acquire_mapping_locked(&mut DEVICE_REGISTRY.lock(), start, span) {
        return Ok(vaddr + paddr.align_offset_4k());
    }

    kplat::mmio::prepare(start.as_usize(), span).map_err(|_| IoMapError::MappingFailed)?;

    let mut kernel_aspace = kernel_layout().lock();
    let mut regions = DEVICE_REGISTRY.lock();

    if let Some(vaddr) = acquire_mapping_locked(&mut regions, start, span) {
        return Ok(vaddr + paddr.align_offset_4k());
    }

    let limit = iomap_window();
    let hint = limit.start;
    let mapped_start = kernel_aspace
        .find_free_area(hint, span, limit, memaddr::PAGE_SIZE_4K)
        .ok_or(IoMapError::NoMemory)?;

    kernel_aspace
        .map_linear(
            mapped_start,
            start,
            span,
            MappingFlags::READ | MappingFlags::WRITE | MappingFlags::DEVICE,
        )
        .map_err(|_| IoMapError::MappingFailed)?;
    // TLB shootdown is already handled by `PageTableMut::finish()`:
    // `map_linear` → `Backend::map` → `pgtbl.modify()` creates a
    // `PageTableMut`, whose `Drop` calls `finish()` →
    // `flush_tlb_all_cpus()` (kernel page table), which does a local
    // flush + IPI broadcast to all online CPUs.

    let region = DeviceRegion {
        paddr: start,
        size: span,
        name,
        vaddr: Some(mapped_start),
    };
    regions.try_reserve(1).map_err(|_| IoMapError::NoMemory)?;
    regions.push(RegionEntry {
        region,
        refs: RegionRefs::Counted(1),
    });
    Ok(mapped_start + paddr.align_offset_4k())
}

/// Release a reference previously taken by [`iomap_device`].
///
/// When the last reference to a runtime mapping is dropped, the virtual mapping
/// is torn down. Pinned (statically registered) regions and addresses that do
/// not belong to any runtime mapping are ignored.
pub fn iounmap(vaddr: VirtAddr) -> Result<(), IoMapError> {
    let mut regions = DEVICE_REGISTRY.lock();
    let Some(index) = regions.iter().position(|entry| {
        let Some(base) = entry.region.vaddr else {
            return false;
        };
        let Some(region_end) = base.checked_add(entry.region.size) else {
            return false;
        };
        matches!(entry.refs, RegionRefs::Counted(_)) && base <= vaddr && vaddr < region_end
    }) else {
        return Ok(());
    };

    let RegionRefs::Counted(count) = regions[index].refs else {
        return Ok(());
    };
    if count > 1 {
        regions[index].refs = RegionRefs::Counted(count - 1);
        return Ok(());
    }

    let entry = regions.swap_remove(index);
    let Some(base) = entry.region.vaddr else {
        return Ok(());
    };
    drop(regions);

    kernel_layout()
        .lock()
        .unmap(base, entry.region.size)
        .map_err(|_| IoMapError::MappingFailed)?;
    // TLB shootdown already handled by `PageTableMut::finish()` via
    // `MmSpace::unmap` → `Backend::unmap` → `pgtbl.modify()` → `Drop`.
    Ok(())
}

fn register_region(region: DeviceRegion) -> Result<(), IoMapError> {
    if region.size == 0 || region.paddr.checked_add(region.size).is_none() {
        return Err(IoMapError::InvalidRange);
    }

    let mut regions = DEVICE_REGISTRY.lock();
    if regions.iter().any(|entry| entry.region == region) {
        return Ok(());
    }

    regions.try_reserve(1).map_err(|_| IoMapError::NoMemory)?;
    regions.push(RegionEntry {
        region,
        refs: RegionRefs::Pinned,
    });
    Ok(())
}

/// Find a registered mapping that covers `[paddr, paddr + size)` and, when it is
/// a runtime reference-counted mapping, take an additional reference.
fn acquire_mapping_locked(
    regions: &mut [RegionEntry],
    paddr: PhysAddr,
    size: usize,
) -> Option<VirtAddr> {
    let req_end = paddr.checked_add(size)?;
    let index = regions.iter().position(|entry| {
        let Some(region_end) = entry.region.paddr.checked_add(entry.region.size) else {
            return false;
        };
        entry.region.vaddr.is_some() && entry.region.paddr <= paddr && region_end >= req_end
    })?;

    let entry = &mut regions[index];
    let mapped_start = entry.region.vaddr?;
    if let RegionRefs::Counted(count) = entry.refs {
        entry.refs = RegionRefs::Counted(count + 1);
    }
    Some(mapped_start + paddr.sub_addr(entry.region.paddr))
}

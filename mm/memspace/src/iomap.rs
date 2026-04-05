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

static DEVICE_REGISTRY: SpinNoIrq<Vec<DeviceRegion>> = SpinNoIrq::new(Vec::new());

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
    DEVICE_REGISTRY.lock().clone().into_iter()
}

/// Map a device MMIO region and register it for diagnostics.
///
/// Drivers are expected to call this during initialization, cache the returned
/// virtual base, and reuse that base from fast paths such as IRQ handlers.
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

    if let Some(vaddr) = find_mapping_locked(&DEVICE_REGISTRY.lock(), start, span) {
        return Ok(vaddr + paddr.align_offset_4k());
    }

    kplat::mmio::prepare(start.as_usize(), span).map_err(|_| IoMapError::MappingFailed)?;

    let mut kernel_aspace = kernel_layout().lock();
    let mut regions = DEVICE_REGISTRY.lock();

    if let Some(vaddr) = find_mapping_locked(&regions, start, span) {
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
    karch::flush_tlb(None);

    let region = DeviceRegion {
        paddr: start,
        size: span,
        name,
        vaddr: Some(mapped_start),
    };
    if regions.contains(&region) {
        return Ok(mapped_start + paddr.align_offset_4k());
    }
    regions.try_reserve(1).map_err(|_| IoMapError::NoMemory)?;
    regions.push(region);
    Ok(mapped_start + paddr.align_offset_4k())
}

fn register_region(region: DeviceRegion) -> Result<(), IoMapError> {
    if region.size == 0 || region.paddr.checked_add(region.size).is_none() {
        return Err(IoMapError::InvalidRange);
    }

    let mut regions = DEVICE_REGISTRY.lock();
    if regions.contains(&region) {
        return Ok(());
    }

    regions.try_reserve(1).map_err(|_| IoMapError::NoMemory)?;
    regions.push(region);
    Ok(())
}

fn find_mapping_locked(regions: &[DeviceRegion], paddr: PhysAddr, size: usize) -> Option<VirtAddr> {
    let req_end = paddr.checked_add(size)?;
    regions.iter().find_map(|region| {
        let mapped_start = region.vaddr?;
        let region_end = region.paddr.checked_add(region.size)?;
        if region.paddr <= paddr && region_end >= req_end {
            Some(mapped_start + paddr.sub_addr(region.paddr))
        } else {
            None
        }
    })
}

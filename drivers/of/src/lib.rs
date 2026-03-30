// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device-tree access helpers.

#![no_std]

use core::ffi::CStr;

use lazyinit::LazyInit;
pub use rs_fdtree::{
    Dice, FdtError, FdtNode, InterruptController, LinuxFdt, MemoryRegion, NodeProperty, RegIter,
};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FirmwareInitError {
    MissingDeviceTreePtr,
    BadDeviceTree(FdtError),
}

static FDT: LazyInit<LinuxFdt<'static>> = LazyInit::new();

fn property_str<'b, 'a>(node: FdtNode<'b, 'a>, name: &str) -> Option<&'a str> {
    let prop = node.property(name)?;
    let cstr = CStr::from_bytes_until_nul(prop.value).ok()?;
    cstr.to_str().ok()
}

pub fn fdt() -> Option<&'static LinuxFdt<'static>> {
    FDT.get()
}

/// Read the total size of the DTB referenced by `ptr`.
///
/// # Safety
///
/// `ptr` must point to a valid, readable DTB blob for the duration of this
/// call.
pub unsafe fn dtb_total_size_from_ptr(ptr: *const u8) -> Result<usize, FirmwareInitError> {
    let fdt = unsafe { LinuxFdt::from_ptr(ptr) }.map_err(FirmwareInitError::BadDeviceTree)?;
    Ok(fdt.total_size())
}

/// Initialize the global DTB handle from a raw pointer.
///
/// # Safety
///
/// `ptr` must point to a valid DTB blob that remains accessible for the rest of
/// the program lifetime.
pub unsafe fn init_device_tree_ptr(ptr: *const u8) -> Result<(), FirmwareInitError> {
    let fdt = unsafe { LinuxFdt::from_ptr(ptr) }.map_err(FirmwareInitError::BadDeviceTree)?;
    FDT.init_once(fdt);
    Ok(())
}

pub fn chosen_bootargs() -> Option<&'static str> {
    let node = fdt()?.all_nodes().find(|node| node.name == "chosen")?;
    property_str(node, "bootargs")
}

pub fn root_model() -> Option<&'static str> {
    let node = fdt()?.all_nodes().find(|node| node.name == "/")?;
    property_str(node, "model")
}

pub fn root_compatible() -> Option<&'static str> {
    fdt()?
        .all_nodes()
        .find(|node| node.name == "/")?
        .compatible()
}

pub fn find_compatible(compatible: &str) -> Option<FdtNode<'static, 'static>> {
    fdt()?
        .all_nodes()
        .find(|node| node.is_compatible(compatible))
}

pub fn interrupt_controller() -> Option<InterruptController<'static, 'static>> {
    fdt()?.interrupt_controller()
}

pub fn dice_region() -> Option<crate::MemoryRegion> {
    fdt()?.dice()?.regions()?.next()
}

fn collect_memory_regions_from_fdt<const N: usize>(
    fdt: &LinuxFdt<'_>,
) -> ([crate::MemoryRegion; N], usize) {
    let mut regions = [crate::MemoryRegion {
        starting_address: core::ptr::null(),
        size: 0,
    }; N];
    let mut count = 0;

    for node in fdt.all_nodes() {
        let is_memory_name = node.name == "memory" || node.name.starts_with("memory@");
        let is_memory_type = node
            .property("device_type")
            .and_then(|prop| CStr::from_bytes_until_nul(prop.value).ok())
            .is_some_and(|device_type| device_type.to_bytes() == b"memory");
        if !is_memory_name && !is_memory_type {
            continue;
        }

        let Some(node_regions) = node.reg() else {
            continue;
        };
        for region in node_regions {
            if count == N {
                return (regions, count);
            }
            regions[count] = region;
            count += 1;
        }
    }

    (regions, count)
}

/// Read `/memory` regions directly from a DTB pointer without touching the
/// global DTB state.
///
/// # Safety
///
/// `ptr` must point to a valid, readable DTB blob for the duration of this
/// call.
pub unsafe fn read_memory_regions_from_ptr<const N: usize>(
    ptr: *const u8,
) -> Result<([crate::MemoryRegion; N], usize), FirmwareInitError> {
    let fdt = unsafe { LinuxFdt::from_ptr(ptr) }.map_err(FirmwareInitError::BadDeviceTree)?;
    Ok(collect_memory_regions_from_fdt(&fdt))
}

pub fn read_memory_regions<const N: usize>() -> ([crate::MemoryRegion; N], usize) {
    fdt().map(collect_memory_regions_from_fdt).unwrap_or((
        [crate::MemoryRegion {
            starting_address: core::ptr::null(),
            size: 0,
        }; N],
        0,
    ))
}

pub fn read_reserved_memory_regions<const N: usize>() -> ([crate::MemoryRegion; N], usize) {
    let mut regions = [crate::MemoryRegion {
        starting_address: core::ptr::null(),
        size: 0,
    }; N];
    let mut count = 0;

    if let Some(fdt) = fdt() {
        for region in fdt.mem_reservations().chain(fdt.reserved_memory_regions()) {
            if region.size == 0 {
                continue;
            }
            assert!(count < N, "too many reserved memory regions in device tree");
            regions[count] = region;
            count += 1;
        }
    }

    if count != 0 {
        regions[..count].sort_unstable_by_key(|region| region.starting_address as usize);

        let mut write = 0;
        for read in 1..count {
            let cur_start = regions[write].starting_address as usize;
            let cur_end = cur_start + regions[write].size;
            let next_start = regions[read].starting_address as usize;
            let next_end = next_start + regions[read].size;

            if next_start <= cur_end {
                regions[write].size = cur_end.max(next_end) - cur_start;
            } else {
                write += 1;
                regions[write] = regions[read];
            }
        }
        count = write + 1;
    }

    (regions, count)
}

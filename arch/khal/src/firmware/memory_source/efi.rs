// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![cfg(not(target_arch = "x86_64"))]

use boot_info::{BootInfo, BootProtocol};
use firmware_handoff::efi::{BootMemmapRef, LinuxEfiBootMemmapHeader, MemoryRegionKind};

use super::{RamRegions, ReservedRegions, append_boot_runtime_reserved, append_dtb_reserved};
use crate::mem::{ReservedKind, ReservedRegion, ReservedSource};

pub(super) fn collect_from_boot_protocol(
    boot_info: &BootInfo,
    ram_regions: &mut RamRegions,
    reserved_regions: &mut ReservedRegions,
) {
    match boot_info.protocol() {
        BootProtocol::Uefi => {
            let ptr = boot_info
                .uefi_memmap_ptr()
                .expect("UEFI memory root selected without a usable memmap handoff");
            collect_uefi_memory(ptr, ram_regions, reserved_regions);
            append_dtb_reserved(boot_info, reserved_regions);
            append_boot_runtime_reserved(boot_info, reserved_regions);
        }
        _ => panic!("non-device-tree platform memory description is not initialized"),
    }
}

fn collect_uefi_memory(
    memmap_ptr: *const u8,
    ram_regions: &mut RamRegions,
    reserved_regions: &mut ReservedRegions,
) {
    // SAFETY: `memmap_ptr` is the bootloader-provided EFI memory-map blob and
    // is interpreted according to the Linux EFI handoff header format.
    let memmap = unsafe { BootMemmapRef::from_ptr(memmap_ptr.cast::<LinuxEfiBootMemmapHeader>()) }
        .expect("invalid Linux EFI boot memmap");

    for entry in memmap.entries() {
        let size = entry.size();
        if size == 0 {
            continue;
        }

        match entry.kind() {
            MemoryRegionKind::Ram => ram_regions
                .push((entry.phys_start(), size))
                .expect("too many boot-protocol RAM regions"),
            MemoryRegionKind::Acpi => super::push_reserved_non_overlapping(
                reserved_regions,
                ReservedRegion::new(
                    entry.phys_start(),
                    size,
                    ReservedKind::Acpi,
                    ReservedSource::BootProtocol,
                    entry.type_name(),
                ),
            ),
            MemoryRegionKind::Persistent => super::push_reserved_non_overlapping(
                reserved_regions,
                ReservedRegion::new(
                    entry.phys_start(),
                    size,
                    ReservedKind::Persistent,
                    ReservedSource::BootProtocol,
                    entry.type_name(),
                ),
            ),
            MemoryRegionKind::Unusable => super::push_reserved_non_overlapping(
                reserved_regions,
                ReservedRegion::new(
                    entry.phys_start(),
                    size,
                    ReservedKind::Unusable,
                    ReservedSource::BootProtocol,
                    entry.type_name(),
                ),
            ),
            MemoryRegionKind::RuntimeServices
            | MemoryRegionKind::Mmio
            | MemoryRegionKind::MmioPortSpace
            | MemoryRegionKind::PalCode
            | MemoryRegionKind::Reserved => super::push_reserved_non_overlapping(
                reserved_regions,
                ReservedRegion::new(
                    entry.phys_start(),
                    size,
                    ReservedKind::Firmware,
                    ReservedSource::BootProtocol,
                    entry.type_name(),
                ),
            ),
        }
    }
}

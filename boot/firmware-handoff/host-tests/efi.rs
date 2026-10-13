// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{ffi::c_void, ptr};

use super::*;

#[test]
fn config_table_lookup_prefers_acpi_20() {
    let tables = [
        ConfigurationTable {
            vendor_guid: ACPI_TABLE_GUID,
            vendor_table: 0x1111usize as *mut c_void,
        },
        ConfigurationTable {
            vendor_guid: ACPI_20_TABLE_GUID,
            vendor_table: 0x2222usize as *mut c_void,
        },
        ConfigurationTable {
            vendor_guid: DEVICE_TREE_GUID,
            vendor_table: 0x3333usize as *mut c_void,
        },
    ];
    let system_table = SystemTable {
        number_of_configuration_table_entries: tables.len(),
        configuration_table: tables.as_ptr() as *mut ConfigurationTable,
        ..SystemTable::default()
    };

    let view = unsafe { RawEfiContext::from_ptr(&system_table) }.unwrap();
    let map = |addr| addr as *const ConfigurationTable;
    assert_eq!(unsafe { view.rsdp_addr(map) }.unwrap(), Some(0x2222));
    assert_eq!(unsafe { view.dtb_addr(map) }.unwrap(), Some(0x3333));
}

#[test]
fn boot_memmap_iter_respects_descriptor_stride() {
    #[repr(C, align(8))]
    struct MemmapBlob {
        header: LinuxEfiBootMemmapHeader,
        descriptors: [u8; 96],
    }

    let mut blob = MemmapBlob {
        header: LinuxEfiBootMemmapHeader {
            map_size: 96,
            desc_size: 48,
            desc_ver: MemoryDescriptor::VERSION,
            map_key: 0,
            buff_size: 96,
        },
        descriptors: [0; 96],
    };

    let first = MemoryDescriptor {
        ty: MemoryType::CONVENTIONAL,
        phys_start: 0x1000,
        virt_start: 0,
        page_count: 2,
        att: 0,
    };
    let second = MemoryDescriptor {
        ty: MemoryType::ACPI_RECLAIM,
        phys_start: 0x8000,
        virt_start: 0,
        page_count: 3,
        att: 0,
    };

    unsafe {
        ptr::write(
            blob.descriptors.as_mut_ptr().cast::<MemoryDescriptor>(),
            first,
        );
        ptr::write(
            blob.descriptors
                .as_mut_ptr()
                .add(48)
                .cast::<MemoryDescriptor>(),
            second,
        );
    }

    let memmap = unsafe { BootMemmapRef::from_ptr(&blob.header) }.unwrap();
    let mut entries = memmap.entries();
    let first = entries.next().unwrap();
    let second = entries.next().unwrap();
    assert!(entries.next().is_none());
    assert_eq!(first.phys_start(), 0x1000);
    assert_eq!(first.size(), 0x2000);
    assert_eq!(first.kind(), MemoryRegionKind::Ram);
    assert_eq!(second.phys_start(), 0x8000);
    assert_eq!(second.size(), 0x3000);
    assert_eq!(second.kind(), MemoryRegionKind::Acpi);
}

#[test]
fn linear_mapping_filter_skips_mmio() {
    let descriptor = MemoryDescriptor {
        ty: MemoryType::MMIO,
        phys_start: 0x1000,
        virt_start: 0,
        page_count: 1,
        att: 0,
    };
    let view = MemoryDescriptorView { descriptor };
    assert!(!view.is_linear_mapping_candidate());
    assert_eq!(view.type_name(), "mmio");
}

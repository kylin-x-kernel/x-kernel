// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{mem, slice, str};

use kaddr_layout::PAGE_OFFSET;

#[derive(Clone, Copy)]
pub struct McfgAllocation {
    pub base_address: u64,
    pub pci_segment: u16,
    pub start_bus: u8,
    pub end_bus: u8,
}

pub fn find_mcfg(rsdp_addr: usize) -> Option<McfgAllocation> {
    if rsdp_addr == 0 {
        kplat::kprintln!("ACPI: missing RSDP address");
        return None;
    }

    let rsdp = ptr_from_addr::<Rsdp>(rsdp_addr);
    if rsdp.signature != *b"RSD PTR " {
        kplat::kprintln!("ACPI: bad RSDP signature at {:#x}", rsdp_addr);
        return None;
    }
    if !checksum_ok(bytes_from_addr(rsdp_addr, 20)) {
        kplat::kprintln!("ACPI: bad RSDP v1 checksum at {:#x}", rsdp_addr);
        return None;
    }

    let rsdp_revision = rsdp.revision;
    let rsdt_addr = rsdp.rsdt_addr;
    let xsdt_addr = rsdp.xsdt_addr;
    let rsdp_length = rsdp.length;
    if rsdp_revision >= 2 && !checksum_ok(bytes_from_addr(rsdp_addr, rsdp_length as usize)) {
        kplat::kprintln!("ACPI: bad RSDP v2 checksum at {:#x}", rsdp_addr);
        return None;
    }
    kplat::kprintln!(
        "ACPI: RSDP revision={} rsdt={:#x} xsdt={:#x}",
        rsdp_revision,
        rsdt_addr,
        xsdt_addr
    );

    let sdt = if rsdp_revision >= 2 && xsdt_addr != 0 {
        find_table_xsdt(xsdt_addr as usize, *b"MCFG")
    } else if rsdt_addr != 0 {
        find_table_rsdt(rsdt_addr as usize, *b"MCFG")
    } else {
        None
    }?;

    parse_mcfg(sdt)
}

fn find_table_xsdt(xsdt_addr: usize, signature: [u8; 4]) -> Option<usize> {
    let header = ptr_from_addr::<AcpiSdtHeader>(xsdt_addr);
    if header.signature != *b"XSDT" {
        kplat::kprintln!("ACPI: bad XSDT signature at {:#x}", xsdt_addr);
        return None;
    }
    if !checksum_ok(sdt_bytes(xsdt_addr, header.length)) {
        kplat::kprintln!("ACPI: bad XSDT checksum at {:#x}", xsdt_addr);
        return None;
    }

    let entries_len =
        (header.length as usize - mem::size_of::<AcpiSdtHeader>()) / mem::size_of::<u64>();
    let entries = unsafe {
        slice::from_raw_parts(
            addr_to_ptr::<u64>(xsdt_addr + mem::size_of::<AcpiSdtHeader>()),
            entries_len,
        )
    };
    kplat::kprintln!("ACPI: XSDT at {:#x} entries={}", xsdt_addr, entries_len);
    for entry in entries.iter().copied() {
        if has_signature(entry as usize, signature) {
            kplat::kprintln!("ACPI: found MCFG at {:#x}", entry);
            return Some(entry as usize);
        }
    }
    kplat::kprintln!("ACPI: MCFG not present in XSDT");
    None
}

fn find_table_rsdt(rsdt_addr: usize, signature: [u8; 4]) -> Option<usize> {
    let header = ptr_from_addr::<AcpiSdtHeader>(rsdt_addr);
    if header.signature != *b"RSDT" {
        kplat::kprintln!("ACPI: bad RSDT signature at {:#x}", rsdt_addr);
        return None;
    }
    if !checksum_ok(sdt_bytes(rsdt_addr, header.length)) {
        kplat::kprintln!("ACPI: bad RSDT checksum at {:#x}", rsdt_addr);
        return None;
    }

    let entries_len =
        (header.length as usize - mem::size_of::<AcpiSdtHeader>()) / mem::size_of::<u32>();
    let entries = unsafe {
        slice::from_raw_parts(
            addr_to_ptr::<u32>(rsdt_addr + mem::size_of::<AcpiSdtHeader>()),
            entries_len,
        )
    };
    kplat::kprintln!("ACPI: RSDT at {:#x} entries={}", rsdt_addr, entries_len);
    for entry in entries.iter().copied() {
        if has_signature(entry as usize, signature) {
            kplat::kprintln!("ACPI: found MCFG at {:#x}", entry);
            return Some(entry as usize);
        }
    }
    kplat::kprintln!("ACPI: MCFG not present in RSDT");
    None
}

fn has_signature(table_addr: usize, signature: [u8; 4]) -> bool {
    let header = ptr_from_addr::<AcpiSdtHeader>(table_addr);
    header.signature == signature && checksum_ok(sdt_bytes(table_addr, header.length))
}

fn parse_mcfg(mcfg_addr: usize) -> Option<McfgAllocation> {
    let header = ptr_from_addr::<AcpiSdtHeader>(mcfg_addr);
    if header.signature != *b"MCFG" {
        kplat::kprintln!("ACPI: table at {:#x} is not MCFG", mcfg_addr);
        return None;
    }

    let entries_start = mcfg_addr + mem::size_of::<AcpiSdtHeader>() + 8;
    let entries_len = (header.length as usize - mem::size_of::<AcpiSdtHeader>() - 8)
        / mem::size_of::<McfgEntryRaw>();
    let entries =
        unsafe { slice::from_raw_parts(addr_to_ptr::<McfgEntryRaw>(entries_start), entries_len) };

    let entry = entries.first()?;
    let base_address = entry.base_address;
    let pci_segment = entry.pci_segment;
    let start_bus = entry.start_bus;
    let end_bus = entry.end_bus;
    kplat::kprintln!(
        "ACPI: MCFG entry base={:#x} seg={} buses={:#x}..={:#x}",
        base_address,
        pci_segment,
        start_bus,
        end_bus
    );
    Some(McfgAllocation {
        base_address,
        pci_segment,
        start_bus,
        end_bus,
    })
}

fn checksum_ok(bytes: &[u8]) -> bool {
    bytes.iter().fold(0u8, |sum, &b| sum.wrapping_add(b)) == 0
}

fn sdt_bytes(addr: usize, length: u32) -> &'static [u8] {
    unsafe { slice::from_raw_parts(addr_to_ptr::<u8>(addr), length as usize) }
}

fn bytes_from_addr(addr: usize, length: usize) -> &'static [u8] {
    unsafe { slice::from_raw_parts(addr_to_ptr::<u8>(addr), length) }
}

fn ptr_from_addr<T>(addr: usize) -> &'static T {
    unsafe { &*addr_to_ptr::<T>(addr) }
}

fn addr_to_ptr<T>(addr: usize) -> *const T {
    let mapped = if addr >= PAGE_OFFSET {
        addr
    } else {
        addr + PAGE_OFFSET
    };
    mapped as *const T
}

#[repr(C, packed)]
struct Rsdp {
    signature: [u8; 8],
    checksum: u8,
    oem_id: [u8; 6],
    revision: u8,
    rsdt_addr: u32,
    length: u32,
    xsdt_addr: u64,
    extended_checksum: u8,
    reserved: [u8; 3],
}

#[repr(C, packed)]
struct AcpiSdtHeader {
    signature: [u8; 4],
    length: u32,
    revision: u8,
    checksum: u8,
    oem_id: [u8; 6],
    oem_table_id: [u8; 8],
    oem_revision: u32,
    creator_id: u32,
    creator_revision: u32,
}

#[repr(C, packed)]
struct McfgEntryRaw {
    base_address: u64,
    pci_segment: u16,
    start_bus: u8,
    end_bus: u8,
    _reserved: u32,
}

const _: () = {
    assert!(mem::size_of::<Rsdp>() == 36);
    assert!(mem::size_of::<AcpiSdtHeader>() == 36);
    assert!(mem::size_of::<McfgEntryRaw>() == 16);
    let _ = str::from_utf8(b"MCFG");
};

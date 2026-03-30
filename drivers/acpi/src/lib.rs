// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ACPI firmware description and table helpers.

#![no_std]

use core::{mem, slice, str};

use kaddr_layout::PAGE_OFFSET;
use kplat::memory::MemRange;
use lazyinit::LazyInit;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcpiDesc {
    rsdp_addr: usize,
}

impl AcpiDesc {
    pub const fn new(rsdp_addr: usize) -> Self {
        Self { rsdp_addr }
    }

    pub const fn rsdp_addr(self) -> usize {
        self.rsdp_addr
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcpiInitError {
    MissingRsdp,
}

#[derive(Debug, Clone, Copy)]
pub struct AcpiTableHeader {
    pub signature: [u8; 4],
    pub length: u32,
    pub revision: u8,
    pub checksum: u8,
    pub oem_id: [u8; 6],
    pub oem_table_id: [u8; 8],
    pub oem_revision: u32,
    pub creator_id: u32,
    pub creator_revision: u32,
}

impl From<AcpiSdtHeader> for AcpiTableHeader {
    fn from(value: AcpiSdtHeader) -> Self {
        Self {
            signature: value.signature,
            length: value.length,
            revision: value.revision,
            checksum: value.checksum,
            oem_id: value.oem_id,
            oem_table_id: value.oem_table_id,
            oem_revision: value.oem_revision,
            creator_id: value.creator_id,
            creator_revision: value.creator_revision,
        }
    }
}

#[derive(Clone, Copy)]
pub struct McfgAllocation {
    pub base_address: u64,
    pub pci_segment: u16,
    pub start_bus: u8,
    pub end_bus: u8,
}

#[derive(Debug, Clone, Copy)]
pub struct MadtInfo {
    pub local_apic_address: u32,
    pub flags: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct LocalApicEntry {
    pub processor_uid: u8,
    pub apic_id: u8,
    pub flags: u32,
}

impl LocalApicEntry {
    pub const fn enabled(self) -> bool {
        (self.flags & 0x1) != 0
    }
}

#[derive(Debug, Clone, Copy)]
pub struct IoApicEntry {
    pub id: u8,
    pub address: u32,
    pub global_system_interrupt_base: u32,
}

#[derive(Debug, Clone, Copy)]
pub enum MadtEntry {
    LocalApic(LocalApicEntry),
    IoApic(IoApicEntry),
}

pub struct MadtEntryIter {
    current: usize,
    end: usize,
}

impl McfgAllocation {
    pub fn ecam_region(self) -> Option<MemRange> {
        let start = self.base_address as usize + ((self.start_bus as usize) << 20);
        let size = ((self.end_bus as usize).checked_sub(self.start_bus as usize)? + 1) << 20;
        Some((start, size))
    }
}

static ACPI_DESC: LazyInit<AcpiDesc> = LazyInit::new();

pub fn init(rsdp_addr: usize) -> Result<(), AcpiInitError> {
    if rsdp_addr == 0 {
        return Err(AcpiInitError::MissingRsdp);
    }
    ACPI_DESC.init_once(AcpiDesc::new(rsdp_addr));
    Ok(())
}

pub fn desc() -> Option<AcpiDesc> {
    ACPI_DESC.get().copied()
}

pub fn rsdp_addr() -> Option<usize> {
    desc().map(AcpiDesc::rsdp_addr)
}

pub fn find_mcfg_from_init() -> Option<McfgAllocation> {
    find_mcfg(rsdp_addr()?)
}

pub fn find_madt() -> Option<(MadtInfo, MadtEntryIter)> {
    let (table_addr, header) = find_table(*b"APIC")?;
    if header.length < (mem::size_of::<AcpiSdtHeader>() + mem::size_of::<MadtHeader>()) as u32 {
        kplat::kprintln!("ACPI: MADT too short at {:#x}", table_addr);
        return None;
    }
    let madt_addr = table_addr + mem::size_of::<AcpiSdtHeader>();
    let madt = ptr_from_addr::<MadtHeader>(madt_addr);
    let info = MadtInfo {
        local_apic_address: madt.local_apic_address,
        flags: madt.flags,
    };
    let entries_start = madt_addr + mem::size_of::<MadtHeader>();
    let entries_end = table_addr + header.length as usize;
    Some((
        info,
        MadtEntryIter {
            current: entries_start,
            end: entries_end,
        },
    ))
}

pub fn find_madt_from_init() -> Option<(MadtInfo, MadtEntryIter)> {
    find_madt_from_rsdp(rsdp_addr()?)
}

pub fn find_madt_from_rsdp(rsdp_addr: usize) -> Option<(MadtInfo, MadtEntryIter)> {
    let (table_addr, header) = find_table_from_rsdp(rsdp_addr, *b"APIC")?;
    if header.length < (mem::size_of::<AcpiSdtHeader>() + mem::size_of::<MadtHeader>()) as u32 {
        kplat::kprintln!("ACPI: MADT too short at {:#x}", table_addr);
        return None;
    }
    let madt_addr = table_addr + mem::size_of::<AcpiSdtHeader>();
    let madt = ptr_from_addr::<MadtHeader>(madt_addr);
    let info = MadtInfo {
        local_apic_address: madt.local_apic_address,
        flags: madt.flags,
    };
    let entries_start = madt_addr + mem::size_of::<MadtHeader>();
    let entries_end = table_addr + header.length as usize;
    Some((
        info,
        MadtEntryIter {
            current: entries_start,
            end: entries_end,
        },
    ))
}

pub fn find_local_apic_address_from_init() -> Option<usize> {
    find_madt_from_init().map(|(info, _)| info.local_apic_address as usize)
}

pub fn find_io_apic_from_init() -> Option<IoApicEntry> {
    let (_, mut entries) = find_madt_from_init()?;
    entries.find_map(|entry| match entry {
        MadtEntry::IoApic(io_apic) => Some(io_apic),
        _ => None,
    })
}

pub fn find_table(signature: [u8; 4]) -> Option<(usize, AcpiTableHeader)> {
    find_table_from_rsdp(rsdp_addr()?, signature)
}

pub fn find_table_from_rsdp(
    rsdp_addr: usize,
    signature: [u8; 4],
) -> Option<(usize, AcpiTableHeader)> {
    let rsdp = validate_rsdp(rsdp_addr)?;
    let table_addr = if rsdp.revision >= 2 && rsdp.xsdt_addr != 0 {
        find_table_xsdt(rsdp.xsdt_addr as usize, signature)
    } else if rsdp.rsdt_addr != 0 {
        find_table_rsdt(rsdp.rsdt_addr as usize, signature)
    } else {
        None
    }?;
    Some((table_addr, read_sdt_header(table_addr).into()))
}

pub fn find_mcfg(rsdp_addr: usize) -> Option<McfgAllocation> {
    let rsdp = validate_rsdp(rsdp_addr)?;
    let rsdp_revision = rsdp.revision;
    let rsdt_addr = rsdp.rsdt_addr;
    let xsdt_addr = rsdp.xsdt_addr;
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

fn validate_rsdp(rsdp_addr: usize) -> Option<&'static Rsdp> {
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

    if rsdp.revision >= 2 {
        if rsdp.length < mem::size_of::<Rsdp>() as u32 {
            kplat::kprintln!("ACPI: short RSDP v2 length at {:#x}", rsdp_addr);
            return None;
        }
        if !checksum_ok(bytes_from_addr(rsdp_addr, rsdp.length as usize)) {
            kplat::kprintln!("ACPI: bad RSDP v2 checksum at {:#x}", rsdp_addr);
            return None;
        }
    }

    Some(rsdp)
}

fn find_table_xsdt(xsdt_addr: usize, signature: [u8; 4]) -> Option<usize> {
    let header = validate_sdt_header(xsdt_addr, Some(*b"XSDT"))?;
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
    let header = validate_sdt_header(rsdt_addr, Some(*b"RSDT"))?;
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
    validate_sdt_header(table_addr, None).is_some_and(|header| header.signature == signature)
}

fn read_sdt_header(table_addr: usize) -> AcpiSdtHeader {
    *ptr_from_addr::<AcpiSdtHeader>(table_addr)
}

impl Iterator for MadtEntryIter {
    type Item = MadtEntry;

    fn next(&mut self) -> Option<Self::Item> {
        while self.current + mem::size_of::<MadtEntryHeader>() <= self.end {
            let header = ptr_from_addr::<MadtEntryHeader>(self.current);
            if header.length < mem::size_of::<MadtEntryHeader>() as u8 {
                kplat::kprintln!("ACPI: invalid MADT entry length {}", header.length);
                return None;
            }

            let entry_len = header.length as usize;
            let entry_addr = self.current;
            let entry_end = match entry_addr.checked_add(entry_len) {
                Some(end) if end <= self.end => end,
                _ => {
                    kplat::kprintln!("ACPI: MADT entry at {:#x} exceeds table bounds", entry_addr);
                    return None;
                }
            };

            self.current = entry_end;

            match header.entry_type {
                0 if entry_len >= mem::size_of::<MadtLocalApic>() => {
                    let entry = ptr_from_addr::<MadtLocalApic>(entry_addr);
                    return Some(MadtEntry::LocalApic(LocalApicEntry {
                        processor_uid: entry.processor_uid,
                        apic_id: entry.apic_id,
                        flags: entry.flags,
                    }));
                }
                1 if entry_len >= mem::size_of::<MadtIoApic>() => {
                    let entry = ptr_from_addr::<MadtIoApic>(entry_addr);
                    return Some(MadtEntry::IoApic(IoApicEntry {
                        id: entry.id,
                        address: entry.address,
                        global_system_interrupt_base: entry.global_system_interrupt_base,
                    }));
                }
                _ => continue,
            }
        }

        None
    }
}

fn parse_mcfg(mcfg_addr: usize) -> Option<McfgAllocation> {
    let header = validate_sdt_header(mcfg_addr, Some(*b"MCFG"))?;
    let entries_base = mem::size_of::<AcpiSdtHeader>() + 8;
    if header.length < entries_base as u32 {
        kplat::kprintln!("ACPI: MCFG too short at {:#x}", mcfg_addr);
        return None;
    }

    let entries_start = mcfg_addr + entries_base;
    let entries_len = (header.length as usize - entries_base) / mem::size_of::<McfgEntryRaw>();
    let entries =
        unsafe { slice::from_raw_parts(addr_to_ptr::<McfgEntryRaw>(entries_start), entries_len) };

    let mut selected = None;
    for entry in entries {
        let alloc = McfgAllocation {
            base_address: entry.base_address,
            pci_segment: entry.pci_segment,
            start_bus: entry.start_bus,
            end_bus: entry.end_bus,
        };
        kplat::kprintln!(
            "ACPI: MCFG entry base={:#x} seg={} buses={:#x}..={:#x}",
            alloc.base_address,
            alloc.pci_segment,
            alloc.start_bus,
            alloc.end_bus
        );
        if alloc.pci_segment == 0 && selected.is_none() {
            selected = Some(alloc);
        }
    }

    if selected.is_none() {
        kplat::kprintln!("ACPI: no usable MCFG segment 0 entry found");
    }
    selected
}

fn checksum_ok(bytes: &[u8]) -> bool {
    bytes.iter().fold(0u8, |sum, &b| sum.wrapping_add(b)) == 0
}

fn validate_sdt_header(
    table_addr: usize,
    expected_signature: Option<[u8; 4]>,
) -> Option<AcpiSdtHeader> {
    let header = read_sdt_header(table_addr);
    if header.length < mem::size_of::<AcpiSdtHeader>() as u32 {
        kplat::kprintln!("ACPI: short SDT length at {:#x}", table_addr);
        return None;
    }
    if let Some(expected) = expected_signature
        && header.signature != expected
    {
        kplat::kprintln!("ACPI: bad SDT signature at {:#x}", table_addr);
        return None;
    }
    if !checksum_ok(sdt_bytes(table_addr, header.length)) {
        kplat::kprintln!("ACPI: bad SDT checksum at {:#x}", table_addr);
        return None;
    }
    Some(header)
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
#[derive(Clone, Copy)]
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

#[repr(C, packed)]
struct MadtHeader {
    local_apic_address: u32,
    flags: u32,
}

#[repr(C, packed)]
struct MadtEntryHeader {
    entry_type: u8,
    length: u8,
}

#[repr(C, packed)]
struct MadtLocalApic {
    header: MadtEntryHeader,
    processor_uid: u8,
    apic_id: u8,
    flags: u32,
}

#[repr(C, packed)]
struct MadtIoApic {
    header: MadtEntryHeader,
    id: u8,
    _reserved: u8,
    address: u32,
    global_system_interrupt_base: u32,
}

const _: () = {
    assert!(mem::size_of::<Rsdp>() == 36);
    assert!(mem::size_of::<AcpiSdtHeader>() == 36);
    assert!(mem::size_of::<McfgEntryRaw>() == 16);
    assert!(mem::size_of::<MadtHeader>() == 8);
    assert!(mem::size_of::<MadtLocalApic>() == 8);
    assert!(mem::size_of::<MadtIoApic>() == 12);
    let _ = str::from_utf8(b"MCFG");
    let _ = str::from_utf8(b"APIC");
};

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ACPI firmware description and table helpers.

#![no_std]

use core::{mem, slice, str};

use kaddr_layout::PAGE_OFFSET;
use lazyinit::LazyInit;

type MemRange = (usize, usize);

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApicInfo {
    pub local_apic_address: usize,
    pub io_apic_address: Option<usize>,
}

/// One memory window advertised by a PCI host bridge `_CRS` resource template.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PciHostMemWindow {
    pub base: u64,
    pub size: u64,
    pub prefetchable: bool,
    pub is_64bit: bool,
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
        let base = usize::try_from(self.base_address).ok()?;
        let start = base.checked_add((self.start_bus as usize) << 20)?;
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
    assert!(
        header.length >= (mem::size_of::<AcpiSdtHeader>() + mem::size_of::<MadtHeader>()) as u32,
        "ACPI MADT is too short"
    );
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
    let rsdp_addr = rsdp_addr().unwrap_or_else(|| panic!("ACPI RSDP is not initialized"));
    find_madt_from_rsdp(rsdp_addr)
}

pub fn find_madt_from_rsdp(rsdp_addr: usize) -> Option<(MadtInfo, MadtEntryIter)> {
    let (table_addr, header) = find_table_from_rsdp(rsdp_addr, *b"APIC")?;
    assert!(
        header.length >= (mem::size_of::<AcpiSdtHeader>() + mem::size_of::<MadtHeader>()) as u32,
        "ACPI MADT is too short"
    );
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

pub fn find_apic_from_init() -> Option<ApicInfo> {
    let (info, entries) = find_madt_from_init()?;
    let io_apic_address = entries.into_iter().find_map(|entry| match entry {
        MadtEntry::IoApic(io_apic) => Some(io_apic.address as usize),
        _ => None,
    });
    Some(ApicInfo {
        local_apic_address: info.local_apic_address as usize,
        io_apic_address,
    })
}

pub fn find_io_apic_from_init() -> Option<IoApicEntry> {
    let (_, mut entries) = find_madt_from_init()?;
    entries.find_map(|entry| match entry {
        MadtEntry::IoApic(io_apic) => Some(io_apic),
        _ => None,
    })
}

/// Best-effort: find the primary PCI host bridge's first non-prefetchable
/// memory window from the DSDT `_CRS` resource template.
///
/// This is **not** a full AML interpreter. It locates the PCI host bridge by
/// scanning the DSDT body for the EISA-encoded `PNP0A08` (PCI Express) or
/// `PNP0A03` (legacy PCI) hardware ID, then searches forward (within a
/// bounded window) for the `_CRS` name and walks the byte stream looking
/// for QWord/DWord `Address Space Descriptor` Large Items (tags `0x8A` /
/// `0x87`). It returns the first memory descriptor whose flags do not
/// indicate prefetchable. This is robust against typical
/// QEMU / firmware-generated DSDTs but should not be relied on for arbitrary
/// platforms — callers must treat its result as advisory.
pub fn find_pci_host_mem_window_from_init() -> Option<PciHostMemWindow> {
    let rsdp = rsdp_addr()?;
    let dsdt_addr = find_dsdt_address(rsdp)?;
    let header = validate_sdt_header(dsdt_addr, Some(*b"DSDT"))?;
    let body = sdt_bytes(dsdt_addr, header.length);
    let body = body.get(mem::size_of::<AcpiSdtHeader>()..)?;
    scan_dsdt_for_pci_mem_window(body)
}

fn find_dsdt_address(rsdp: usize) -> Option<usize> {
    let (fadt_addr, header) = find_table_from_rsdp(rsdp, *b"FACP")?;
    let body = sdt_bytes(fadt_addr, header.length);
    // FADT layout: DSDT @ offset 40 (u32), X_DSDT @ offset 140 (u64, ACPI 2.0+).
    if header.length >= 148 {
        let xdsdt = u64::from_le_bytes(body.get(140..148)?.try_into().ok()?);
        if xdsdt != 0 {
            return Some(xdsdt as usize);
        }
    }
    if header.length >= 44 {
        let dsdt = u32::from_le_bytes(body.get(40..44)?.try_into().ok()?);
        if dsdt != 0 {
            return Some(dsdt as usize);
        }
    }
    None
}

fn scan_dsdt_for_pci_mem_window(body: &[u8]) -> Option<PciHostMemWindow> {
    // EISA-encoded PNP0A08 / PNP0A03.
    const PNP0A08: [u8; 4] = [0x41, 0xD0, 0x0A, 0x08];
    const PNP0A03: [u8; 4] = [0x41, 0xD0, 0x0A, 0x03];
    const CRS_NAME: [u8; 4] = *b"_CRS";

    // The PCI host device's `_CRS` is normally within a few KB of its `_HID`
    // declaration. Bound the search to keep this O(N) with small constants.
    const CRS_SEARCH_WINDOW: usize = 16 * 1024;
    const RES_SCAN_WINDOW: usize = 8 * 1024;

    let hid_pos = find_subsequence(body, &PNP0A08).or_else(|| find_subsequence(body, &PNP0A03))?;
    let crs_search_end = hid_pos.saturating_add(CRS_SEARCH_WINDOW).min(body.len());
    let crs_pos =
        find_subsequence(body.get(hid_pos..crs_search_end)?, &CRS_NAME).map(|p| p + hid_pos)?;

    let scan_start = crs_pos + CRS_NAME.len();
    let scan_end = scan_start.saturating_add(RES_SCAN_WINDOW).min(body.len());
    let bytes = body.get(scan_start..scan_end)?;

    parse_resource_template_for_mem_window(bytes)
}

fn parse_resource_template_for_mem_window(bytes: &[u8]) -> Option<PciHostMemWindow> {
    // Resource Type byte (offset 0 of payload):
    //   0 = Memory Range, 1 = IO Range, 2 = Bus Number Range
    // Type Specific Flags byte (offset 2 of payload, memory variant):
    //   bit 0   : Read/Write
    //   bits 2-1: Memory attributes (00=NC, 01=Cacheable,
    //             10=Cacheable+Combine, 11=Cacheable+Prefetchable)
    const RES_TYPE_MEM: u8 = 0;
    const PREFETCHABLE_MASK: u8 = 0b0000_0110;
    const PREFETCHABLE_VAL: u8 = 0b0000_0110;

    let mut i = 0;
    while i < bytes.len() {
        let tag = bytes[i];
        match tag {
            // DWord Address Space Descriptor: tag (1) + length (2) + body.
            0x87 => {
                let body_start = i + 3;
                let len = u16::from_le_bytes([*bytes.get(i + 1)?, *bytes.get(i + 2)?]) as usize;
                let body_end = body_start + len;
                if body_end > bytes.len() {
                    return None;
                }
                let p = &bytes[body_start..body_end];
                if p.len() < 23 {
                    return None;
                }
                if p[0] == RES_TYPE_MEM {
                    let prefetchable = (p[2] & PREFETCHABLE_MASK) == PREFETCHABLE_VAL;
                    let min = u32::from_le_bytes(p[7..11].try_into().ok()?);
                    let len = u32::from_le_bytes(p[19..23].try_into().ok()?);
                    if !prefetchable && len > 0 && min > 0 {
                        return Some(PciHostMemWindow {
                            base: min as u64,
                            size: len as u64,
                            prefetchable: false,
                            is_64bit: false,
                        });
                    }
                }
                i = body_end;
            }
            // QWord Address Space Descriptor: tag (1) + length (2) + body.
            0x8A => {
                let body_start = i + 3;
                let len = u16::from_le_bytes([*bytes.get(i + 1)?, *bytes.get(i + 2)?]) as usize;
                let body_end = body_start + len;
                if body_end > bytes.len() {
                    return None;
                }
                let p = &bytes[body_start..body_end];
                if p.len() < 43 {
                    return None;
                }
                if p[0] == RES_TYPE_MEM {
                    let prefetchable = (p[2] & PREFETCHABLE_MASK) == PREFETCHABLE_VAL;
                    let min = u64::from_le_bytes(p[11..19].try_into().ok()?);
                    let len = u64::from_le_bytes(p[35..43].try_into().ok()?);
                    if !prefetchable && len > 0 && min > 0 {
                        return Some(PciHostMemWindow {
                            base: min,
                            size: len,
                            prefetchable: false,
                            is_64bit: true,
                        });
                    }
                }
                i = body_end;
            }
            // End Tag (Small Item, tag = 0x79).
            0x79 => return None,
            _ => i += 1,
        }
    }
    None
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

pub fn find_table(signature: [u8; 4]) -> Option<(usize, AcpiTableHeader)> {
    let rsdp_addr = rsdp_addr().unwrap_or_else(|| panic!("ACPI RSDP is not initialized"));
    find_table_from_rsdp(rsdp_addr, signature)
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
        panic!("ACPI RSDP does not reference XSDT or RSDT")
    }
    .unwrap_or_else(|| {
        let signature = core::str::from_utf8(&signature).unwrap_or("????");
        panic!("required ACPI table {signature} is missing")
    });
    Some((table_addr, read_sdt_header(table_addr).into()))
}

pub fn find_mcfg(rsdp_addr: usize) -> Option<McfgAllocation> {
    let rsdp = validate_rsdp(rsdp_addr)?;
    let rsdp_revision = rsdp.revision;
    let rsdt_addr = rsdp.rsdt_addr;
    let xsdt_addr = rsdp.xsdt_addr;

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
        panic!("ACPI RSDP address is zero");
    }

    let rsdp = ptr_from_addr::<Rsdp>(rsdp_addr);
    if rsdp.signature != *b"RSD PTR " {
        panic!("ACPI RSDP signature is invalid");
    }
    if !checksum_ok(bytes_from_addr(rsdp_addr, 20)) {
        panic!("ACPI RSDP checksum is invalid");
    }

    if rsdp.revision >= 2 {
        if rsdp.length < mem::size_of::<Rsdp>() as u32 {
            panic!("ACPI RSDP length is too short");
        }
        if !checksum_ok(bytes_from_addr(rsdp_addr, rsdp.length as usize)) {
            panic!("ACPI RSDP extended checksum is invalid");
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
    for entry in entries.iter().copied() {
        if has_signature(entry as usize, signature) {
            return Some(entry as usize);
        }
    }
    let signature = core::str::from_utf8(&signature).unwrap_or("????");
    panic!("required ACPI table {signature} is missing from XSDT")
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
    for entry in entries.iter().copied() {
        if has_signature(entry as usize, signature) {
            return Some(entry as usize);
        }
    }
    let signature = core::str::from_utf8(&signature).unwrap_or("????");
    panic!("required ACPI table {signature} is missing from RSDT")
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
                panic!("ACPI MADT entry is too short");
            }

            let entry_len = header.length as usize;
            let entry_addr = self.current;
            let entry_end = match entry_addr.checked_add(entry_len) {
                Some(end) if end <= self.end => end,
                _ => panic!("ACPI MADT entry exceeds table bounds"),
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
        if alloc.pci_segment == 0 && selected.is_none() {
            selected = Some(alloc);
        }
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
        panic!("ACPI SDT header is too short");
    }
    if let Some(expected) = expected_signature
        && header.signature != expected
    {
        let expected = core::str::from_utf8(&expected).unwrap_or("????");
        panic!("ACPI SDT signature does not match expected {expected}");
    }
    if !checksum_ok(sdt_bytes(table_addr, header.length)) {
        panic!("ACPI SDT checksum is invalid");
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

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ACPI firmware description and table helpers.

#![allow(
    clippy::panic,
    reason = "CG-021-no-panic-on-reachable-failure: case (b): boot-critical firmware corruption \
              in RSDP and SDT/MADT fixed headers still panics by contract; MADT entry framing \
              reports typed errors instead, and new code remains reviewed"
)]
#![no_std]

use core::{mem, slice, str};

use kaddr_layout::PAGE_OFFSET;
use lazyinit::LazyInit;

type MemRange = (usize, usize);

/// ACPI boot descriptor: the firmware-passed RSDP physical address.
///
/// All later table lookups walk from this root. Stored once by
/// [`init`] and read back through [`desc`] / [`rsdp_addr`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcpiDesc {
    rsdp_addr: usize,
}

impl AcpiDesc {
    /// Wraps a raw RSDP physical address.
    pub const fn new(rsdp_addr: usize) -> Self {
        Self { rsdp_addr }
    }

    /// Returns the RSDP physical address.
    pub const fn rsdp_addr(self) -> usize {
        self.rsdp_addr
    }
}

/// Failure to initialize the ACPI subsystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcpiInitError {
    /// `init` was called with a zero RSDP address.
    MissingRsdp,
}

/// Safe snapshot of an ACPI System Description Table header.
///
/// Produced only after the raw header passed signature and checksum
/// validation.
#[derive(Debug, Clone, Copy)]
pub struct AcpiTableHeader {
    /// Four-character table signature (e.g. `APIC`, `MCFG`, `FACP`).
    pub signature: [u8; 4],
    /// Total table length in bytes, header included.
    pub length: u32,
    /// Table revision number.
    pub revision: u8,
    /// Whole-table checksum byte (sum with the body must be 0 mod 256).
    pub checksum: u8,
    /// Six-character OEM identifier.
    pub oem_id: [u8; 6],
    /// Eight-character OEM table identifier.
    pub oem_table_id: [u8; 8],
    /// OEM revision of the table.
    pub oem_revision: u32,
    /// Utility that created the table.
    pub creator_id: u32,
    /// Revision of the creating utility.
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

/// One PCI MMIO configuration space allocation from the MCFG table.
#[derive(Clone, Copy)]
pub struct McfgAllocation {
    /// 64-bit physical base address of the ECAM window.
    pub base_address: u64,
    /// PCI segment (domain) this allocation covers.
    pub pci_segment: u16,
    /// First bus number covered.
    pub start_bus: u8,
    /// Last bus number covered (inclusive).
    pub end_bus: u8,
}

/// MADT table header facts: the legacy Local APIC address and flags.
#[derive(Debug, Clone, Copy)]
pub struct MadtInfo {
    /// Physical address of the local APIC register block.
    pub local_apic_address: u32,
    /// Raw MADT flags (e.g. PC-AT compatibility bit).
    pub flags: u32,
}

/// One memory window advertised by a PCI host bridge `_CRS` resource
/// template.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PciHostMemWindow {
    /// CPU physical base address of the window.
    pub base: u64,
    /// Window size in bytes.
    pub size: u64,
    /// Whether the descriptor marks the window prefetchable.
    pub prefetchable: bool,
    /// Whether the window came from a QWord (64-bit) descriptor.
    pub is_64bit: bool,
}

/// The ACPI PM1a control block from the FADT (ACPI 5.0 §4.8.3.2), reached
/// through port IO: the register sleep-state requests are written to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pm1aControlBlock {
    /// IO-port number of the control block.
    pub port: u16,
    /// `PM1_CNT_LEN` in bytes: the register width the firmware declares.
    pub length: u8,
}

/// One processor entry from the MADT (type 0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalApicEntry {
    /// Processor UID as listed by the firmware (often the ACPI UID).
    pub processor_uid: u8,
    /// APIC id used to address the processor's local APIC.
    pub apic_id: u8,
    /// Raw MADT flags; bit 0 is the enabled bit (see [`LocalApicEntry::enabled`]).
    pub flags: u32,
}

impl LocalApicEntry {
    /// Returns whether the firmware marked this processor usable.
    pub const fn enabled(self) -> bool {
        (self.flags & 0x1) != 0
    }
}

/// One I/O APIC entry from the MADT (type 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IoApicEntry {
    /// I/O APIC id as listed by the firmware.
    pub id: u8,
    /// Physical address of the I/O APIC register block.
    pub address: u32,
    /// First global system interrupt number routed through this APIC.
    pub global_system_interrupt_base: u32,
}

/// One interrupt source override from the MADT (type 2).
///
/// The flags are kept raw: trigger and polarity policy belongs to the
/// architecture integration layer, not to ACPI table parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InterruptSourceOverride {
    /// Bus the source interrupt belongs to (0 is ISA).
    pub bus: u8,
    /// Bus-relative source interrupt number.
    pub source: u8,
    /// Global system interrupt it is routed to.
    pub gsi: u32,
    /// Raw MPS INTI flags (polarity in bits 0:1, trigger mode in bits 2:3).
    pub flags: u16,
}

/// A parsed MADT entry, narrowed to the kinds the kernel consumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MadtEntry {
    /// Processor entry (MADT type 0).
    LocalApic(LocalApicEntry),
    /// I/O APIC entry (MADT type 1).
    IoApic(IoApicEntry),
    /// Interrupt source override (MADT type 2).
    InterruptSourceOverride(InterruptSourceOverride),
}

/// Why a MADT entry could not be parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MadtParseErrorKind {
    /// The entry header or declared body runs past the entry region.
    TruncatedEntry,
    /// The entry declares a length smaller than the two-byte entry header.
    InvalidLength,
    /// A known entry is shorter than its fixed structure.
    ShortEntry,
}

/// A MADT framing error and the table-relative offset it was detected at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MadtParseError {
    /// Offset from the start of the MADT table.
    pub offset: usize,
    /// What made the entry unparseable.
    pub kind: MadtParseErrorKind,
}

/// Iterator over the variable-length entries of a validated MADT.
///
/// Entry kinds the kernel does not model are skipped. A malformed entry
/// yields one [`MadtParseError`] and then ends the iteration; it is never
/// treated as a panic or silently dropped.
#[derive(Clone, Copy)]
pub struct MadtEntryIter<'a> {
    entries: &'a [u8],
    /// Table-relative offset of the first byte of `entries`.
    base_offset: usize,
    offset: usize,
    done: bool,
}

impl<'a> MadtEntryIter<'a> {
    /// Iterates over a raw MADT entry region.
    ///
    /// `base_offset` is the table-relative offset of `entries[0]` and is
    /// added to offsets reported in [`MadtParseError`].
    pub const fn new(entries: &'a [u8], base_offset: usize) -> Self {
        Self {
            entries,
            base_offset,
            offset: 0,
            done: false,
        }
    }
}

impl McfgAllocation {
    /// Computes the mapped `(base, size)` ECAM range for this allocation:
    /// 1 MiB per bus, starting at `start_bus`. Returns `None` when the
    /// address does not fit `usize` or the bus arithmetic overflows.
    pub fn ecam_region(self) -> Option<MemRange> {
        let base = usize::try_from(self.base_address).ok()?;
        let start = base.checked_add((self.start_bus as usize) << 20)?;
        let size = ((self.end_bus as usize).checked_sub(self.start_bus as usize)? + 1) << 20;
        Some((start, size))
    }
}

static ACPI_DESC: LazyInit<AcpiDesc> = LazyInit::new();

/// Stores the boot RSDP physical address; must run before any lookup.
///
/// The first successful call wins; later calls with different addresses
/// have no effect.
///
/// # Errors
///
/// Returns [`AcpiInitError::MissingRsdp`] when `rsdp_addr` is zero.
pub fn init(rsdp_addr: usize) -> Result<(), AcpiInitError> {
    if rsdp_addr == 0 {
        return Err(AcpiInitError::MissingRsdp);
    }
    ACPI_DESC.init_once(AcpiDesc::new(rsdp_addr));
    Ok(())
}

/// Returns the boot descriptor, or `None` before [`init`].
pub fn desc() -> Option<AcpiDesc> {
    ACPI_DESC.get().copied()
}

/// Returns the boot RSDP physical address, or `None` before [`init`].
pub fn rsdp_addr() -> Option<usize> {
    desc().map(AcpiDesc::rsdp_addr)
}

/// Finds the first segment-0 MCFG allocation using the boot RSDP.
pub fn find_mcfg_from_init() -> Option<McfgAllocation> {
    find_mcfg(rsdp_addr()?)
}

/// Finds the MADT from the boot RSDP.
///
/// Returns `None` when ACPI was never initialized or the firmware has no
/// MADT. A structurally invalid MADT header still panics.
pub fn find_madt_from_init() -> Option<(MadtInfo, MadtEntryIter<'static>)> {
    find_madt_from_rsdp(rsdp_addr()?)
}

/// Finds the MADT from an explicit RSDP address.
///
/// Returns `None` when the firmware has no MADT. A structurally invalid
/// MADT header still panics; corrupt entry framing is reported by the
/// returned iterator.
pub fn find_madt_from_rsdp(rsdp_addr: usize) -> Option<(MadtInfo, MadtEntryIter<'static>)> {
    let (table_addr, header) = find_table_optional_from_rsdp(rsdp_addr, *b"APIC")?;
    let header_len = mem::size_of::<AcpiSdtHeader>();
    assert!(
        header.length >= (header_len + MADT_HEADER_SIZE) as u32,
        "ACPI MADT is too short"
    );
    let table = sdt_bytes(table_addr, header.length);
    let body = table.get(header_len..)?;
    let info = MadtInfo {
        local_apic_address: u32::from_le_bytes(body.get(0..4)?.try_into().ok()?),
        flags: u32::from_le_bytes(body.get(4..8)?.try_into().ok()?),
    };
    let entries_offset = header_len + MADT_HEADER_SIZE;
    let entries = body.get(MADT_HEADER_SIZE..)?;
    Some((info, MadtEntryIter::new(entries, entries_offset)))
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

/// Finds the FADT PM1a control block from the boot-initialized RSDP.
///
/// The block is an IO-port register on this platform: an ACPI 2.0+
/// `X_PM1a_CNT_BLK` generic address in system-IO space is preferred, with
/// the legacy 32-bit `PM1a_CNT_BLK` field as fallback. `None` when ACPI is
/// uninitialized, the FADT is absent, or neither field names a usable port
/// — callers decide what power-off means without one.
///
/// Unlike [`find_table_from_rsdp`], a missing FADT is `None` rather than a
/// panic: power terminals reach this after the rest of the system is torn
/// down and must stay panic-free.
pub fn find_pm1a_control_block_from_init() -> Option<Pm1aControlBlock> {
    let rsdp = validate_rsdp(rsdp_addr()?)?;
    let fadt_addr = if rsdp.revision >= 2 && rsdp.xsdt_addr != 0 {
        find_table_xsdt(rsdp.xsdt_addr as usize, *b"FACP")
    } else if rsdp.rsdt_addr != 0 {
        find_table_rsdt(rsdp.rsdt_addr as usize, *b"FACP")
    } else {
        None
    }?;
    let header = read_sdt_header(fadt_addr);
    let body = sdt_bytes(fadt_addr, header.length);

    // FADT layout: PM1a_CNT_BLK @ offset 64 (u32), PM1_CNT_LEN @ offset 89
    // (u8), X_PM1a_CNT_BLK GAS @ offset 172 (address space u8, address u64
    // at +8) — ACPI 2.0+ only, hence the length and revision guards.
    let port = if header.length >= 188 && header.revision >= 2 {
        let address_space = *body.get(172)?;
        let address = u64::from_le_bytes(body.get(180..188)?.try_into().ok()?);
        if address_space == 1 && address != 0 && address <= u16::MAX as u64 {
            address as u16
        } else {
            legacy_pm1a_port(body)?
        }
    } else {
        legacy_pm1a_port(body)?
    };
    let length = if header.length >= 90 {
        *body.get(89)?
    } else {
        0
    };
    Some(Pm1aControlBlock { port, length })
}

/// The legacy 32-bit `PM1a_CNT_BLK` field, which names an IO port directly.
fn legacy_pm1a_port(body: &[u8]) -> Option<u16> {
    let port = u32::from_le_bytes(body.get(64..68)?.try_into().ok()?);
    (port != 0 && port <= u16::MAX as u32).then_some(port as u16)
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

/// Finds a table by four-character signature from the boot RSDP.
///
/// # Panics
///
/// Panics when ACPI was never initialized, when the RSDP fails
/// validation, when the RSDP names neither XSDT nor RSDT, or when the
/// requested table is missing from the root table.
pub fn find_table(signature: [u8; 4]) -> Option<(usize, AcpiTableHeader)> {
    let rsdp_addr = rsdp_addr().unwrap_or_else(|| panic!("ACPI RSDP is not initialized"));
    find_table_from_rsdp(rsdp_addr, signature)
}

/// Finds a table by signature starting from an explicit RSDP address.
///
/// # Panics
///
/// Panics when the RSDP fails validation, when it names neither XSDT nor
/// RSDT, or when the requested table is missing from the root table.
pub fn find_table_from_rsdp(
    rsdp_addr: usize,
    signature: [u8; 4],
) -> Option<(usize, AcpiTableHeader)> {
    find_table_optional_from_rsdp(rsdp_addr, signature).or_else(|| {
        let signature = core::str::from_utf8(&signature).unwrap_or("????");
        panic!("required ACPI table {signature} is missing")
    })
}

/// Looks up a table without treating an absent table as fatal.
///
/// The RSDP and root table must still be structurally valid; only an
/// absent table signature returns `None`.
fn find_table_optional_from_rsdp(
    rsdp_addr: usize,
    signature: [u8; 4],
) -> Option<(usize, AcpiTableHeader)> {
    let rsdp = validate_rsdp(rsdp_addr)?;
    let table_addr = if rsdp.revision >= 2 && rsdp.xsdt_addr != 0 {
        find_table_xsdt_optional(rsdp.xsdt_addr as usize, signature)
    } else if rsdp.rsdt_addr != 0 {
        find_table_rsdt_optional(rsdp.rsdt_addr as usize, signature)
    } else {
        panic!("ACPI RSDP does not reference XSDT or RSDT")
    }?;
    Some((table_addr, read_sdt_header(table_addr).into()))
}

/// Finds the MCFG table from an explicit RSDP address and parses the first
/// segment-0 allocation.
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
    find_table_xsdt_optional(xsdt_addr, signature)
}

fn find_table_xsdt_optional(xsdt_addr: usize, signature: [u8; 4]) -> Option<usize> {
    let header = validate_sdt_header(xsdt_addr, Some(*b"XSDT"))?;
    let table = sdt_bytes(xsdt_addr, header.length);
    let entries = table.get(mem::size_of::<AcpiSdtHeader>()..)?;
    for entry in entries.chunks_exact(mem::size_of::<u64>()) {
        let entry = u64::from_le_bytes(entry.try_into().ok()?);
        if has_signature(entry as usize, signature) {
            return Some(entry as usize);
        }
    }
    None
}

fn find_table_rsdt(rsdt_addr: usize, signature: [u8; 4]) -> Option<usize> {
    find_table_rsdt_optional(rsdt_addr, signature)
}

fn find_table_rsdt_optional(rsdt_addr: usize, signature: [u8; 4]) -> Option<usize> {
    let header = validate_sdt_header(rsdt_addr, Some(*b"RSDT"))?;
    let table = sdt_bytes(rsdt_addr, header.length);
    let entries = table.get(mem::size_of::<AcpiSdtHeader>()..)?;
    for entry in entries.chunks_exact(mem::size_of::<u32>()) {
        let entry = u32::from_le_bytes(entry.try_into().ok()?);
        if has_signature(entry as usize, signature) {
            return Some(entry as usize);
        }
    }
    None
}

fn has_signature(table_addr: usize, signature: [u8; 4]) -> bool {
    validate_sdt_header(table_addr, None).is_some_and(|header| header.signature == signature)
}

fn read_sdt_header(table_addr: usize) -> AcpiSdtHeader {
    *ptr_from_addr::<AcpiSdtHeader>(table_addr)
}

impl Iterator for MadtEntryIter<'_> {
    type Item = Result<MadtEntry, MadtParseError>;

    fn next(&mut self) -> Option<Self::Item> {
        while !self.done {
            let remaining = self.entries.get(self.offset..)?;
            if remaining.is_empty() {
                self.done = true;
                return None;
            }
            let offset = self.base_offset + self.offset;
            if remaining.len() < ENTRY_HEADER_SIZE {
                self.done = true;
                return Some(Err(MadtParseError {
                    offset,
                    kind: MadtParseErrorKind::TruncatedEntry,
                }));
            }

            let entry_len = remaining[1] as usize;
            if entry_len < ENTRY_HEADER_SIZE {
                self.done = true;
                return Some(Err(MadtParseError {
                    offset,
                    kind: MadtParseErrorKind::InvalidLength,
                }));
            }
            let Some(entry) = remaining.get(..entry_len) else {
                self.done = true;
                return Some(Err(MadtParseError {
                    offset,
                    kind: MadtParseErrorKind::TruncatedEntry,
                }));
            };
            self.offset += entry_len;

            let parsed = match remaining[0] {
                MADT_TYPE_LOCAL_APIC => parse_local_apic(entry),
                MADT_TYPE_IO_APIC => parse_io_apic(entry),
                MADT_TYPE_INTERRUPT_SOURCE_OVERRIDE => parse_interrupt_source_override(entry),
                // Entries the kernel does not model are still framed, so skip
                // them without interpreting their payload.
                _ => continue,
            };
            match parsed {
                Ok(parsed) => return Some(Ok(parsed)),
                Err(kind) => {
                    // A malformed known entry invalidates the rest of the
                    // stream: its length no longer proves where the next
                    // entry starts.
                    self.done = true;
                    return Some(Err(MadtParseError { offset, kind }));
                }
            }
        }

        None
    }
}

fn parse_local_apic(entry: &[u8]) -> Result<MadtEntry, MadtParseErrorKind> {
    let Some(body) = entry.get(..LOCAL_APIC_ENTRY_SIZE) else {
        return Err(MadtParseErrorKind::ShortEntry);
    };
    Ok(MadtEntry::LocalApic(LocalApicEntry {
        processor_uid: body[2],
        apic_id: body[3],
        flags: u32::from_le_bytes([body[4], body[5], body[6], body[7]]),
    }))
}

fn parse_io_apic(entry: &[u8]) -> Result<MadtEntry, MadtParseErrorKind> {
    let Some(body) = entry.get(..IO_APIC_ENTRY_SIZE) else {
        return Err(MadtParseErrorKind::ShortEntry);
    };
    Ok(MadtEntry::IoApic(IoApicEntry {
        id: body[2],
        address: u32::from_le_bytes([body[4], body[5], body[6], body[7]]),
        global_system_interrupt_base: u32::from_le_bytes([body[8], body[9], body[10], body[11]]),
    }))
}

fn parse_interrupt_source_override(entry: &[u8]) -> Result<MadtEntry, MadtParseErrorKind> {
    let Some(body) = entry.get(..INTERRUPT_SOURCE_OVERRIDE_ENTRY_SIZE) else {
        return Err(MadtParseErrorKind::ShortEntry);
    };
    Ok(MadtEntry::InterruptSourceOverride(
        InterruptSourceOverride {
            bus: body[2],
            source: body[3],
            gsi: u32::from_le_bytes([body[4], body[5], body[6], body[7]]),
            flags: u16::from_le_bytes([body[8], body[9]]),
        },
    ))
}

fn parse_mcfg(mcfg_addr: usize) -> Option<McfgAllocation> {
    let header = validate_sdt_header(mcfg_addr, Some(*b"MCFG"))?;
    let entries_base = mem::size_of::<AcpiSdtHeader>() + 8;
    if header.length < entries_base as u32 {
        return None;
    }

    let entries_start = mcfg_addr + entries_base;
    let entries_len = (header.length as usize - entries_base) / mem::size_of::<McfgEntryRaw>();
    // SAFETY: `validate_sdt_header` checked that the MCFG payload is large
    // enough, so the entries region is in-bounds for `entries_len` records.
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
    // SAFETY: Callers pass validated ACPI table addresses and lengths obtained
    // from firmware headers, so the resulting byte slice stays in-bounds.
    unsafe { slice::from_raw_parts(addr_to_ptr::<u8>(addr), length as usize) }
}

fn bytes_from_addr(addr: usize, length: usize) -> &'static [u8] {
    // SAFETY: Callers pass validated firmware ranges, so the resulting byte
    // slice stays in-bounds for `length` bytes.
    unsafe { slice::from_raw_parts(addr_to_ptr::<u8>(addr), length) }
}

fn ptr_from_addr<T>(addr: usize) -> &'static T {
    // SAFETY: Callers pass validated firmware structure addresses mapped into
    // the kernel address space, so dereferencing this typed pointer is sound.
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

const MADT_HEADER_SIZE: usize = 8;
const ENTRY_HEADER_SIZE: usize = 2;
const LOCAL_APIC_ENTRY_SIZE: usize = 8;
const IO_APIC_ENTRY_SIZE: usize = 12;
const INTERRUPT_SOURCE_OVERRIDE_ENTRY_SIZE: usize = 10;

const MADT_TYPE_LOCAL_APIC: u8 = 0;
const MADT_TYPE_IO_APIC: u8 = 1;
const MADT_TYPE_INTERRUPT_SOURCE_OVERRIDE: u8 = 2;

const _: () = {
    assert!(mem::size_of::<Rsdp>() == 36);
    assert!(mem::size_of::<AcpiSdtHeader>() == 36);
    assert!(mem::size_of::<McfgEntryRaw>() == 16);
    assert!(str::from_utf8(b"MCFG").is_ok());
    assert!(str::from_utf8(b"APIC").is_ok());
};

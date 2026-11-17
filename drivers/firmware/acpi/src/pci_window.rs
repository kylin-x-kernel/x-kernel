// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Transitional PCI host memory-window probe.
//!
//! This is a bounded byte scan of the DSDT for the PCI host bridge `_CRS`
//! resource template. It is **not** an AML interpreter and its result is
//! advisory. It is compatibility debt: remove it only once a real PCI
//! root-bridge address-resource query exists, the `pci_bar_range` consumers
//! are migrated, and behavior is verified. Link `_CRS` IRQ evaluation is a
//! different fact and does not supersede this scan.

use super::{
    desc,
    fadt::find_dsdt_address,
    raw::sdt_bytes,
    table::{SDT_HEADER_SIZE, validate_sdt_header},
};

/// One memory window advertised by a PCI host bridge `_CRS` resource
/// template.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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
    let rsdp = desc::rsdp_addr()?;
    let dsdt_addr = find_dsdt_address(rsdp)?;
    let header = validate_sdt_header(dsdt_addr, Some(*b"DSDT"))?;
    let body = sdt_bytes(dsdt_addr, header.length);
    let body = body.get(SDT_HEADER_SIZE..)?;
    scan_dsdt_for_pci_mem_window(body)
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
    use acpi_rs::sdt::borrowed::resource::{address_descriptor, is_end_tag};

    let mut remaining = bytes;
    while !remaining.is_empty() {
        if is_end_tag(remaining) {
            return None;
        }
        if let Some((resource, size)) = address_descriptor(remaining).ok()? {
            if resource.resource_type == 0
                && !resource.is_prefetchable()
                && resource.length != 0
                && resource.minimum != 0
            {
                return Some(PciHostMemWindow {
                    base: resource.minimum,
                    size: resource.length,
                    prefetchable: false,
                    is_64bit: resource.is_64bit,
                });
            }
            remaining = &remaining[size..];
        } else {
            remaining = &remaining[1..];
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

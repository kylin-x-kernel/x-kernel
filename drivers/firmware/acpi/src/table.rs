// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Boot address access and required/optional lookup policy.
//! The vendor library owns all RSDP/SDT decoding and root-table traversal.

use acpi_rs::sdt::borrowed::{self, Error, Table};

use super::{desc, raw::bytes_from_addr};

/// Safe snapshot of an ACPI System Description Table header.
///
/// Produced only after the raw header passed signature and checksum
/// validation.
#[derive(Clone, Copy, Debug)]
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

impl From<acpi_rs::sdt::SdtHeader> for AcpiTableHeader {
    fn from(value: acpi_rs::sdt::SdtHeader) -> Self {
        Self {
            signature: value.signature.to_bytes(),
            length: value.length,
            revision: value.revision,
            checksum: value.checksum,
            oem_id: value.oem_id,
            oem_table_id: value.oem_table_id,
            oem_revision: value.oem_revision,
            creator_id: u32::from_le_bytes(value.creator_id),
            creator_revision: value.creator_revision,
        }
    }
}

pub(super) const SDT_HEADER_SIZE: usize = borrowed::HEADER_SIZE;

fn read(address: u64, length: usize) -> Result<&'static [u8], Error> {
    let address = usize::try_from(address).map_err(|_| Error::InvalidAddress)?;
    let mapped = if address >= kaddr_layout::PAGE_OFFSET {
        address
    } else {
        address
            .checked_add(kaddr_layout::PAGE_OFFSET)
            .ok_or(Error::InvalidAddress)?
    };
    if length > isize::MAX as usize || mapped.checked_add(length).is_none() {
        return Err(Error::InvalidAddress);
    }
    Ok(bytes_from_addr(address, length))
}

/// Finds a required table from the initialized boot RSDP.
/// Panics on uninitialized ACPI, missing required data, or corrupt firmware.
pub fn find_table(signature: [u8; 4]) -> Option<(usize, AcpiTableHeader)> {
    let address = desc::rsdp_addr().unwrap_or_else(|| panic!("ACPI RSDP is not initialized"));
    find_table_from_rsdp(address, signature)
}

/// Finds a required table from an explicit early-boot RSDP.
/// Panics on missing required data or corrupt firmware.
pub fn find_table_from_rsdp(
    address: usize,
    signature: [u8; 4],
) -> Option<(usize, AcpiTableHeader)> {
    let (address, table) = find_optional(address, signature)
        .unwrap_or_else(|| panic!("required ACPI table {signature:?} is missing"));
    Some((address, table.header().into()))
}

pub(super) fn find_optional(address: usize, signature: [u8; 4]) -> Option<(usize, Table<'static>)> {
    borrowed::find_table(address as u64, signature, read)
        .unwrap_or_else(|error| panic!("invalid ACPI tables: {error:?}"))
        .map(|(address, table)| (address as usize, table))
}

/// Compatibility probes historically allow an RSDP with neither root address.
/// Malformed existing roots still retain the fatal validation policy.
pub(super) fn probe_optional_root(address: usize, signature: [u8; 4]) -> Option<Table<'static>> {
    match borrowed::find_table(address as u64, signature, read) {
        Ok(table) => table.map(|(_, table)| table),
        Err(Error::MissingRoot) => None,
        Err(error) => panic!("invalid ACPI tables: {error:?}"),
    }
}

pub(super) fn validate_sdt_header(
    address: usize,
    signature: Option<[u8; 4]>,
) -> Option<AcpiTableHeader> {
    let table = borrowed::read_table(address as u64, &mut read)
        .unwrap_or_else(|error| panic!("invalid ACPI table: {error:?}"));
    if let Some(signature) = signature {
        table
            .require_signature(signature)
            .unwrap_or_else(|error| panic!("invalid ACPI table signature: {error:?}"));
    }
    Some(table.header().into())
}

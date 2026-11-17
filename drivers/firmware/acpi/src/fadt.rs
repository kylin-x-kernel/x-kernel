// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Power-port compatibility policy and DSDT selection over vendor FADT access.

use acpi_rs::sdt::borrowed::fadt::Fadt;

use super::{desc, table};

/// PM1a control register selected for the kernel's port-I/O power path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Pm1aControlBlock {
    /// Nonzero I/O port representable by the platform.
    pub port: u16,
    /// Firmware PM1 control length in bytes.
    pub length: u8,
}

/// Selects a usable system-I/O PM1a port, preferring the extended address.
/// Missing ACPI or FADT returns None. Corrupt RSDP/root/SDT data still
/// follows the existing fatal validation policy; this is not panic-free.
pub fn find_pm1a_control_block_from_init() -> Option<Pm1aControlBlock> {
    let table = table::probe_optional_root(desc::rsdp_addr()?, *b"FACP")?;
    let fadt = Fadt::new(table).ok()?;
    let extended = fadt.extended_pm1a_control_address().and_then(|gas| {
        (gas.address_space == 1)
            .then(|| port(gas.address))
            .flatten()
    });
    let port = extended.or_else(|| port(fadt.legacy_pm1a_control_address()? as u64))?;
    Some(Pm1aControlBlock {
        port,
        length: fadt.pm1_control_length().unwrap_or(0),
    })
}

fn port(address: u64) -> Option<u16> {
    let port = u16::try_from(address).ok()?;
    (port != 0).then_some(port)
}

/// A missing FADT remains fatal for the legacy required DSDT query.
pub(super) fn find_dsdt_address(rsdp: usize) -> Option<usize> {
    let (_, table) = table::find_optional(rsdp, *b"FACP")
        .unwrap_or_else(|| panic!("required ACPI FADT is missing"));
    usize::try_from(Fadt::new(table).ok()?.dsdt_address()?).ok()
}

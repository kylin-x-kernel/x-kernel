// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Boot MADT lookup; protocol decoding belongs to the vendor library.

pub use acpi_rs::sdt::borrowed::madt::{
    InterruptSourceOverride, IoApicEntry, LocalApicEntry, MadtEntry, MadtEntryIter, MadtInfo,
    MadtParseError, MadtParseErrorKind,
};

use super::{desc, table};

/// Finds an optional MADT from the boot RSDP.
/// Corrupt table framing retains the fatal boot policy.
pub fn find_madt_from_init() -> Option<(MadtInfo, MadtEntryIter<'static>)> {
    find_madt_from_rsdp(desc::rsdp_addr()?)
}

/// Finds an optional MADT before or after global ACPI initialization.
/// Corrupt fixed headers panic; entry errors are returned by the iterator.
pub fn find_madt_from_rsdp(rsdp_addr: usize) -> Option<(MadtInfo, MadtEntryIter<'static>)> {
    let (_, table) = table::find_optional(rsdp_addr, *b"APIC")?;
    Some(
        acpi_rs::sdt::borrowed::madt::decode(table)
            .unwrap_or_else(|error| panic!("invalid ACPI MADT: {error:?}")),
    )
}

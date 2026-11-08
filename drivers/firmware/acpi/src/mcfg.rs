// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Compatibility selection of the first segment-zero ECAM allocation.

pub use acpi_rs::sdt::borrowed::mcfg::McfgAllocation;
use acpi_rs::sdt::borrowed::mcfg::allocations;

use super::{desc, table};

/// Finds the first segment-zero allocation from the boot RSDP.
pub fn find_mcfg_from_init() -> Option<McfgAllocation> {
    find_mcfg(desc::rsdp_addr()?)
}

/// Finds the first segment-zero allocation without requiring global initialization.
/// Invalid root tables retain the fatal boot policy; malformed MCFG bodies
/// are unavailable to this compatibility query.
pub fn find_mcfg(rsdp_addr: usize) -> Option<McfgAllocation> {
    let table = table::probe_optional_root(rsdp_addr, *b"MCFG")?;
    allocations(table)
        .ok()?
        .find(|allocation| allocation.pci_segment == 0)
}

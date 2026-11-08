//! Allocation-free MCFG records; segment selection belongs to the caller.

use super::{Error, Table, u16_at, u64_at};

/// One PCI ECAM allocation.
#[derive(Debug, Clone, Copy)]
pub struct McfgAllocation {
    pub base_address: u64,
    pub pci_segment: u16,
    pub start_bus: u8,
    pub end_bus: u8,
}

impl McfgAllocation {
    /// Computes the covered ECAM range without address or bus-range truncation.
    pub fn ecam_region(self) -> Option<(usize, usize)> {
        let base = usize::try_from(self.base_address).ok()?;
        let start = base.checked_add((self.start_bus as usize) << 20)?;
        let size = ((self.end_bus as usize).checked_sub(self.start_bus as usize)? + 1) << 20;
        start.checked_add(size - 1)?;
        Some((start, size))
    }
}

/// Validates the reserved prefix and complete entry framing before iteration.
pub fn allocations(table: Table<'_>) -> Result<impl Iterator<Item = McfgAllocation> + '_, Error> {
    table.require_signature(*b"MCFG")?;
    let entries = table.body().get(8..).ok_or(Error::Truncated)?;
    if entries.len() % 16 != 0 {
        return Err(Error::InvalidLength);
    }
    Ok(entries.chunks_exact(16).map(|entry| McfgAllocation {
        base_address: u64_at(entry, 0).unwrap(),
        pci_segment: u16_at(entry, 8).unwrap(),
        start_bus: entry[10],
        end_bus: entry[11],
    }))
}

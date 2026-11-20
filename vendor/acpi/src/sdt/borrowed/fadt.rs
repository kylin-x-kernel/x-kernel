//! Revision- and length-bounded FADT access, independent of physical mappings.

use super::{Error, Table, u32_at, u64_at};
use crate::address::RawGenericAddress;

/// FADT view that does not require a modern full-sized FADT structure.
pub struct Fadt<'a>(Table<'a>);

impl<'a> Fadt<'a> {
    /// Checks the FADT signature.
    pub fn new(table: Table<'a>) -> Result<Self, Error> {
        table.require_signature(*b"FACP")?;
        Ok(Self(table))
    }

    /// Returns the legacy PM1a control address, when the field exists.
    pub fn legacy_pm1a_control_address(&self) -> Option<u32> {
        u32_at(self.0.bytes(), 64).ok()
    }

    /// Returns the PM1 control register length, when the field exists.
    pub fn pm1_control_length(&self) -> Option<u8> {
        self.0.bytes().get(89).copied()
    }

    /// Returns the extended GAS only when revision and complete extent allow it.
    pub fn extended_pm1a_control_address(&self) -> Option<RawGenericAddress> {
        if self.0.header().revision < 2 {
            return None;
        }
        let gas = self.0.bytes().get(172..184)?;
        Some(RawGenericAddress {
            address_space: gas[0],
            bit_width: gas[1],
            bit_offset: gas[2],
            access_size: gas[3],
            address: u64_at(gas, 4).ok()?,
        })
    }

    /// Selects the nonzero extended FACS address, otherwise the legacy address.
    pub fn firmware_control_address(&self) -> Option<u64> {
        if self.0.header().revision >= 2
            && let Ok(address) = u64_at(self.0.bytes(), 132)
            && address != 0
        {
            return Some(address);
        }
        let address = u32_at(self.0.bytes(), 36).ok()? as u64;
        (address != 0).then_some(address)
    }

    /// Selects a nonzero extended DSDT address, otherwise the legacy address.
    pub fn dsdt_address(&self) -> Option<u64> {
        if self.0.header().revision >= 2
            && let Ok(address) = u64_at(self.0.bytes(), 140)
            && address != 0
        {
            return Some(address);
        }
        let address = u32_at(self.0.bytes(), 40).ok()? as u64;
        (address != 0).then_some(address)
    }
}

//! Bounded address-resource decoding for static compatibility consumers.
//! This does not evaluate AML or identify the owner of a resource template.

use super::{Error, u16_at, u32_at, u64_at};

/// Raw facts from a DWord or QWord address-space descriptor.
pub struct AddressDescriptor {
    pub resource_type: u8,
    pub type_flags: u8,
    pub minimum: u64,
    pub length: u64,
    pub is_64bit: bool,
}

impl AddressDescriptor {
    /// Whether a memory descriptor is marked prefetchable.
    pub fn is_prefetchable(&self) -> bool {
        self.resource_type == 0 && self.type_flags & 6 == 6
    }
}

/// Decodes an address descriptor at the beginning of a byte slice.
/// Unknown tags return None; known truncated descriptors return an error.
/// The returned size includes any optional resource-source suffix.
pub fn address_descriptor(bytes: &[u8]) -> Result<Option<(AddressDescriptor, usize)>, Error> {
    let Some(&tag) = bytes.first() else { return Ok(None) };
    let is_64bit = match tag {
        0x87 => false,
        0x8a => true,
        _ => return Ok(None),
    };
    let size = u16_at(bytes, 1)? as usize + 3;
    let body = bytes.get(3..size).ok_or(Error::Truncated)?;
    let minimum_length = if is_64bit { 43 } else { 23 };
    if body.len() < minimum_length {
        return Err(Error::Truncated);
    }
    let (minimum, length) = if is_64bit {
        (u64_at(body, 11)?, u64_at(body, 35)?)
    } else {
        (u32_at(body, 7)? as u64, u32_at(body, 19)? as u64)
    };
    Ok(Some((AddressDescriptor { resource_type: body[0], type_flags: body[2], minimum, length, is_64bit }, size)))
}

/// Recognizes the small EndTag item used to terminate a resource template.
pub fn is_end_tag(bytes: &[u8]) -> bool {
    bytes.first() == Some(&0x79)
}

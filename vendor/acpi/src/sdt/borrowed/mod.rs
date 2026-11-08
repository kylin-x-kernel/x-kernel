//! Strict table decoding from borrowed byte slices, without allocation or AML.
//!
//! Unlike typed physical mappings, these views accept short revision-dependent
//! tables and unaligned input. The caller owns address accessibility and lifetime;
//! this module owns format validation and never chooses a kernel failure policy.

pub mod fadt;
pub mod madt;
pub mod mcfg;
pub mod resource;

use super::SdtHeader;

/// Size of the common SDT header.
pub const HEADER_SIZE: usize = core::mem::size_of::<SdtHeader>();

/// Malformed table data or an inaccessible address supplied by the reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Truncated,
    InvalidLength,
    InvalidSignature,
    InvalidChecksum,
    MissingRoot,
    InvalidAddress,
}

/// A checksum-validated SDT, bounded to its declared length.
#[derive(Clone, Copy)]
pub struct Table<'a> {
    bytes: &'a [u8],
}

/// Reads the declared length from a complete fixed header.
pub fn table_length(bytes: &[u8]) -> Result<usize, Error> {
    let header = bytes.get(..HEADER_SIZE).ok_or(Error::Truncated)?;
    let length = u32_at(header, 4)? as usize;
    if length < HEADER_SIZE {
        return Err(Error::InvalidLength);
    }
    Ok(length)
}

impl<'a> Table<'a> {
    /// Validates minimum length, declared extent, and checksum.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, Error> {
        let length = table_length(bytes)?;
        let bytes = bytes.get(..length).ok_or(Error::Truncated)?;
        checksum(bytes)?;
        Ok(Self { bytes })
    }

    /// Returns an unaligned-safe copy of the vendor's existing header type.
    pub fn header(self) -> SdtHeader {
        SdtHeader {
            signature: super::Signature(self.bytes[0..4].try_into().unwrap()),
            length: u32::from_le_bytes(self.bytes[4..8].try_into().unwrap()),
            revision: self.bytes[8],
            checksum: self.bytes[9],
            oem_id: self.bytes[10..16].try_into().unwrap(),
            oem_table_id: self.bytes[16..24].try_into().unwrap(),
            oem_revision: u32::from_le_bytes(self.bytes[24..28].try_into().unwrap()),
            creator_id: self.bytes[28..32].try_into().unwrap(),
            creator_revision: u32::from_le_bytes(self.bytes[32..36].try_into().unwrap()),
        }
    }

    /// Validates the table kind before interpreting its body.
    pub fn require_signature(self, signature: [u8; 4]) -> Result<(), Error> {
        if self.bytes[..4] != signature {
            return Err(Error::InvalidSignature);
        }
        Ok(())
    }

    /// Returns the bytes following the fixed SDT header.
    pub fn body(self) -> &'a [u8] {
        &self.bytes[HEADER_SIZE..]
    }

    /// Returns the complete validated table.
    pub fn bytes(self) -> &'a [u8] {
        self.bytes
    }
}

fn checksum(bytes: &[u8]) -> Result<(), Error> {
    if bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)) != 0 {
        return Err(Error::InvalidChecksum);
    }
    Ok(())
}

pub(super) fn u16_at(bytes: &[u8], offset: usize) -> Result<u16, Error> {
    let field = bytes.get(offset..offset + 2).ok_or(Error::Truncated)?;
    Ok(u16::from_le_bytes([field[0], field[1]]))
}

pub(super) fn u32_at(bytes: &[u8], offset: usize) -> Result<u32, Error> {
    let field = bytes.get(offset..offset + 4).ok_or(Error::Truncated)?;
    Ok(u32::from_le_bytes([field[0], field[1], field[2], field[3]]))
}

pub(super) fn u64_at(bytes: &[u8], offset: usize) -> Result<u64, Error> {
    let field = bytes.get(offset..offset + 8).ok_or(Error::Truncated)?;
    Ok(u64::from_le_bytes(field.try_into().map_err(|_| Error::Truncated)?))
}

/// Reads and validates an SDT through caller-owned address access.
pub fn read_table<'a>(
    address: u64,
    read: &mut impl FnMut(u64, usize) -> Result<&'a [u8], Error>,
) -> Result<Table<'a>, Error> {
    if address == 0 {
        return Err(Error::InvalidAddress);
    }
    let length = table_length(read(address, HEADER_SIZE)?)?;
    Table::parse(read(address, length)?)
}

/// Walks a validated RSDP and root table without allocating or retaining mappings.
///
/// Both RSDP checksums are required for revision 2+, XSDT is preferred when
/// nonzero, and a partial root entry is rejected rather than silently ignored.
/// Absence is distinct from malformed firmware. Readers must keep returned
/// slices alive for the whole returned table lifetime.
pub fn find_table<'a>(
    rsdp_address: u64,
    signature: [u8; 4],
    mut read: impl FnMut(u64, usize) -> Result<&'a [u8], Error>,
) -> Result<Option<(u64, Table<'a>)>, Error> {
    if rsdp_address == 0 {
        return Err(Error::InvalidAddress);
    }
    let prefix = read(rsdp_address, 20)?;
    let prefix = prefix.get(..20).ok_or(Error::Truncated)?;
    if &prefix[..8] != b"RSD PTR " {
        return Err(Error::InvalidSignature);
    }
    checksum(prefix)?;
    let mut root_address = u32_at(prefix, 16)? as u64;
    let mut entry_size = 4;
    let mut root_signature = *b"RSDT";
    if prefix[15] >= 2 {
        let fixed = read(rsdp_address, 36)?;
        let length = u32_at(fixed, 20)? as usize;
        if length < 36 {
            return Err(Error::InvalidLength);
        }
        let extended = read(rsdp_address, length)?;
        checksum(extended.get(..length).ok_or(Error::Truncated)?)?;
        let xsdt = u64_at(extended, 24)?;
        if xsdt != 0 {
            root_address = xsdt;
            entry_size = 8;
            root_signature = *b"XSDT";
        }
    }
    if root_address == 0 {
        return Err(Error::MissingRoot);
    }
    let root = read_table(root_address, &mut read)?;
    root.require_signature(root_signature)?;
    if root.body().len() % entry_size != 0 {
        return Err(Error::InvalidLength);
    }
    for entry in root.body().chunks_exact(entry_size) {
        let address = if entry_size == 8 { u64_at(entry, 0)? } else { u32_at(entry, 0)? as u64 };
        let table = read_table(address, &mut read)?;
        if table.require_signature(signature).is_ok() {
            return Ok(Some((address, table)));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests;

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Strict, allocation-free property reads shared by DT binding interpreters.

use crate::{FdtNode, NodeProperty};

#[cfg(feature = "alloc")]
pub(crate) fn resource_names<'b, 'a: 'b>(
    node: FdtNode<'b, 'a>,
    property: &str,
) -> Result<impl Iterator<Item = &'a str> + Clone, (Option<usize>, PropertyError)> {
    let bytes = match node.property_strict(property) {
        Ok(property) => property.value,
        Err(PropertyError::Missing) => return Ok("".split_terminator('\0')),
        Err(reason) => return Err((None, reason)),
    };
    for (index, part) in bytes.split_inclusive(|byte| *byte == 0).enumerate() {
        part.strip_suffix(&[0])
            .and_then(|part| core::str::from_utf8(part).ok())
            .filter(|name| !name.is_empty())
            .ok_or((Some(index), PropertyError::InvalidEncoding))?;
    }
    if bytes.is_empty() {
        return Err((Some(0), PropertyError::InvalidEncoding));
    }
    let text = core::str::from_utf8(bytes).map_err(|_| (None, PropertyError::InvalidEncoding))?;
    Ok(text.split_terminator('\0'))
}

/// A borrowed sequence of big-endian 32-bit cells.
///
/// Device-tree properties carry cells as four-byte big-endian words. This
/// view keeps the payload borrowed and decodes each cell on access, so it
/// stays allocation-free and does not depend on host alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cells<'a> {
    bytes: &'a [u8],
}

impl<'a> Cells<'a> {
    /// Validates a property payload as a complete cell sequence.
    pub(crate) fn from_bytes(bytes: &'a [u8]) -> Result<Self, PropertyError> {
        if !bytes.len().is_multiple_of(4) {
            return Err(PropertyError::InvalidEncoding);
        }
        Ok(Self { bytes })
    }

    /// Returns the number of cells.
    pub fn len(&self) -> usize {
        self.bytes.len() / 4
    }

    /// Returns whether the sequence has no cells.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Returns cell `index`, or `None` when out of range.
    pub fn get(&self, index: usize) -> Option<u32> {
        let start = index.checked_mul(4)?;
        let end = start.checked_add(4)?;
        let bytes = self.bytes.get(start..end)?;
        Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// Iterates the cells in firmware order.
    pub fn iter(&self) -> impl Iterator<Item = u32> + 'a {
        let bytes = self.bytes;
        bytes
            .chunks_exact(4)
            .map(|cell| u32::from_be_bytes([cell[0], cell[1], cell[2], cell[3]]))
    }

    /// Returns a sub-sequence of `len` cells starting at `start`.
    ///
    /// Returns `None` when the range is outside this sequence.
    pub fn slice(&self, start: usize, len: usize) -> Option<Self> {
        let start = start.checked_mul(4)?;
        let end = start.checked_add(len.checked_mul(4)?)?;
        Some(Self {
            bytes: self.bytes.get(start..end)?,
        })
    }

    /// Decodes every cell into `output` and returns the number written.
    ///
    /// # Errors
    ///
    /// Returns [`PropertyError::BufferTooSmall`] without modifying `output`
    /// when it cannot hold the complete sequence.
    pub fn read_into(&self, output: &mut [u32]) -> Result<usize, PropertyError> {
        if output.len() < self.len() {
            return Err(PropertyError::BufferTooSmall {
                required: self.len(),
            });
        }
        for (value, bytes) in output.iter_mut().zip(self.bytes.chunks_exact(4)) {
            *value = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        }
        Ok(self.len())
    }

    /// Returns the borrowed big-endian payload.
    pub fn as_bytes(&self) -> &'a [u8] {
        self.bytes
    }
}

/// Failure to read a complete property with the requested shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropertyError {
    /// No property has this name.
    Missing,
    /// More than one property has this name on the node.
    Duplicate,
    /// The bytes are not a complete value of the requested encoding.
    InvalidEncoding,
    /// A list was supplied where a single value was required.
    WrongType,
    /// The output is too short; it has not been modified.
    BufferTooSmall {
        /// Required number of u32 cells, not bytes.
        required: usize,
    },
}

impl<'a> NodeProperty<'a> {
    /// Read one NUL-terminated UTF-8 string, rejecting additional strings.
    pub fn read_string(self) -> Result<&'a str, PropertyError> {
        let text = self.string_payload()?;
        if text.contains('\0') {
            return Err(PropertyError::WrongType);
        }
        Ok(text)
    }

    /// Validate the entire string list before returning its borrowed iterator.
    ///
    /// Empty string elements are preserved; binding-specific readers decide
    /// whether they are permitted. An empty byte property is not a string list.
    pub fn read_strings(self) -> Result<impl Iterator<Item = &'a str>, PropertyError> {
        Ok(self.string_payload()?.split('\0'))
    }

    /// Read exactly one big-endian u32 cell, never silently taking a prefix.
    pub fn read_u32(self) -> Result<u32, PropertyError> {
        if !self.value.len().is_multiple_of(4) {
            return Err(PropertyError::InvalidEncoding);
        }
        let bytes = self
            .value
            .try_into()
            .map_err(|_| PropertyError::WrongType)?;
        Ok(u32::from_be_bytes(bytes))
    }

    /// Read every cell; malformed input or short output leaves output unchanged.
    ///
    /// Empty properties are valid empty arrays. Extra output cells are untouched.
    pub fn read_u32_array(self, output: &mut [u32]) -> Result<usize, PropertyError> {
        if !self.value.len().is_multiple_of(4) {
            return Err(PropertyError::InvalidEncoding);
        }
        let required = self.value.len() / 4;
        if output.len() < required {
            return Err(PropertyError::BufferTooSmall { required });
        }
        for (value, bytes) in output.iter_mut().zip(self.value.chunks_exact(4)) {
            *value = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        }
        Ok(required)
    }

    /// Borrow this property as a validated big-endian cell sequence.
    ///
    /// # Errors
    ///
    /// Returns [`PropertyError::InvalidEncoding`] when the payload length is
    /// not a multiple of four bytes.
    pub fn cells(self) -> Result<Cells<'a>, PropertyError> {
        Cells::from_bytes(self.value)
    }

    fn string_payload(self) -> Result<&'a str, PropertyError> {
        let bytes = self
            .value
            .strip_suffix(&[0])
            .ok_or(PropertyError::InvalidEncoding)?;
        core::str::from_utf8(bytes).map_err(|_| PropertyError::InvalidEncoding)
    }
}

impl<'b, 'a: 'b> FdtNode<'b, 'a> {
    /// Interpret this node's status without applying ancestor or population policy.
    ///
    /// Missing status, `ok` and `okay` mean available. Other strings mean
    /// unavailable; malformed or duplicate properties return an error. This is
    /// allocation-free and does not imply that a controller or driver is ready.
    pub fn is_available(self) -> Result<bool, PropertyError> {
        match self.read_string("status") {
            Ok(status) => Ok(matches!(status, "ok" | "okay")),
            Err(PropertyError::Missing) => Ok(true),
            Err(error) => Err(error),
        }
    }

    /// Read a uniquely named property, distinguishing absence from duplication.
    ///
    /// Scans this node's properties only; no tree index or allocation is required.
    pub fn property_strict(self, name: &str) -> Result<NodeProperty<'a>, PropertyError> {
        let mut matches = self.properties().filter(|property| property.name == name);
        let property = matches.next().ok_or(PropertyError::Missing)?;
        if matches.next().is_some() {
            return Err(PropertyError::Duplicate);
        }
        Ok(property)
    }

    /// Read a uniquely named, single NUL-terminated UTF-8 string.
    pub fn read_string(self, name: &str) -> Result<&'a str, PropertyError> {
        self.property_strict(name)?.read_string()
    }

    /// Validate all string elements before publishing a borrowed iterator.
    pub fn read_strings(self, name: &str) -> Result<impl Iterator<Item = &'a str>, PropertyError> {
        self.property_strict(name)?.read_strings()
    }

    /// Read exactly one cell, distinguishing missing, malformed and list values.
    pub fn read_u32(self, name: &str) -> Result<u32, PropertyError> {
        self.property_strict(name)?.read_u32()
    }

    /// Read all cells, leaving output unchanged when the property cannot be read.
    pub fn read_u32_array(self, name: &str, output: &mut [u32]) -> Result<usize, PropertyError> {
        self.property_strict(name)?.read_u32_array(output)
    }
}

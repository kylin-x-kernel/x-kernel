// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Numeric DT address translation, without kernel mappings or device ownership.

use alloc::vec::Vec;

use crate::{FdtNode, PropertyError};

/// One complete reg interval translated into CPU physical address space.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MmioResource<'a> {
    /// CPU physical start address in bytes, independent of host pointer width.
    pub base: u64,
    /// Interval length in bytes.
    pub size: u64,
    /// Optional borrowed reg-names entry; duplicates remain intact.
    pub name: Option<&'a str>,
}

/// Why a numeric address declaration could not be interpreted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AddressErrorReason {
    /// Missing, malformed or duplicated property.
    Property(PropertyError),
    /// The bus requires a non-numeric address translator.
    UnsupportedBus,
    /// Only one- and two-cell numeric addresses/sizes are supported.
    UnsupportedCells(u32),
    /// A reg declaration has no structural parent.
    MissingParent,
    /// A declared interval has zero size.
    ZeroSize,
    /// Address arithmetic or declared cell width overflowed.
    Overflow,
    /// No ranges window covers the complete resource.
    OutsideRanges,
    /// There are more names than reg tuples.
    ExcessNames,
}

/// An address interpretation error referring only to the input tree.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AddressError {
    /// Structure-block offset of the offending device or ancestor node.
    pub node_offset: u32,
    /// Property being interpreted.
    pub property: &'static str,
    /// Tuple/name index, or None for scalar metadata.
    pub index: Option<usize>,
    /// Rejection reason; this is never an IRQ/provider readiness dependency.
    pub reason: AddressErrorReason,
}

impl<'b, 'a: 'b> FdtNode<'b, 'a> {
    /// Collect CPU MMIO intervals for ordinary numeric buses, in reg order.
    ///
    /// Allocates result metadata, not hardware resources. Any error discards
    /// the entire provisional result. Cell widths default to 2/1 at each bus;
    /// PCI/ISA encodings are unsupported. Missing ranges rejects translation,
    /// empty ranges is identity, and each window must contain the whole interval.
    /// This stricter interval policy differs from translating only its start.
    /// Missing/empty reg yields no resources unless reg-names is present.
    /// Structural parent queries scan the tree; this is a discovery operation.
    pub fn mmio_resources(self) -> Result<Vec<MmioResource<'a>>, AddressError> {
        let reg = match self.property_strict("reg") {
            Ok(property) => property.value,
            Err(PropertyError::Missing) => &[],
            Err(error) => return Err(self.address_error("reg", None, error.into())),
        };
        let names = self.mmio_names()?;
        let mut resources = Vec::new();
        if reg.is_empty() {
            if !names.is_empty() {
                return Err(self.address_error(
                    "reg-names",
                    Some(0),
                    AddressErrorReason::ExcessNames,
                ));
            }
            return Ok(resources);
        }
        let parent = self
            .parent()
            .ok_or_else(|| self.address_error("reg", None, AddressErrorReason::MissingParent))?;
        parent.numeric_bus()?;
        let address_bytes = parent.address_cell_bytes("#address-cells", 2)?;
        let size_bytes = parent.address_cell_bytes("#size-cells", 1)?;
        let tuple_bytes = address_bytes + size_bytes;
        self.address_complete_tuples("reg", reg, tuple_bytes)?;
        let count = reg.len() / tuple_bytes;
        if names.len() > count {
            return Err(self.address_error(
                "reg-names",
                Some(count),
                AddressErrorReason::ExcessNames,
            ));
        }
        for (index, tuple) in reg.chunks_exact(tuple_bytes).enumerate() {
            let address = number(&tuple[..address_bytes]);
            let size = number(&tuple[address_bytes..]);
            self.address_interval_end("reg", index, (address, size), address_bytes)?;
            let base = parent.translate_mmio(address, size, index)?;
            resources.push(MmioResource {
                base,
                size,
                name: names.get(index).copied(),
            });
        }
        Ok(resources)
    }

    fn mmio_names(self) -> Result<Vec<&'a str>, AddressError> {
        crate::property::resource_names(self, "reg-names")
            .map(Iterator::collect)
            .map_err(|(index, reason)| self.address_error("reg-names", index, reason.into()))
    }

    fn numeric_bus(self) -> Result<(), AddressError> {
        if matches!(
            self.name.split('@').next().unwrap_or(""),
            "pci" | "pcie" | "isa"
        ) {
            return Err(self.address_error("reg", None, AddressErrorReason::UnsupportedBus));
        }
        match self.read_string("device_type") {
            Ok("pci" | "pciex" | "vci" | "ht" | "isa") => {
                Err(self.address_error("device_type", None, AddressErrorReason::UnsupportedBus))
            }
            Ok(_) | Err(PropertyError::Missing) => Ok(()),
            Err(error) => Err(self.address_error("device_type", None, error.into())),
        }
    }

    fn address_cell_bytes(
        self,
        property: &'static str,
        default: u32,
    ) -> Result<usize, AddressError> {
        let cells = match self.read_u32(property) {
            Ok(cells) => cells,
            Err(PropertyError::Missing) => default,
            Err(PropertyError::WrongType) => {
                return Err(self.address_error(
                    property,
                    None,
                    PropertyError::InvalidEncoding.into(),
                ));
            }
            Err(error) => return Err(self.address_error(property, None, error.into())),
        };
        match cells {
            1 | 2 => Ok(cells as usize * 4),
            _ => {
                Err(self.address_error(property, None, AddressErrorReason::UnsupportedCells(cells)))
            }
        }
    }

    fn address_complete_tuples(
        self,
        property: &'static str,
        bytes: &[u8],
        tuple_bytes: usize,
    ) -> Result<(), AddressError> {
        if !bytes.len().is_multiple_of(tuple_bytes) {
            return Err(self.address_error(
                property,
                Some(bytes.len() / tuple_bytes),
                PropertyError::InvalidEncoding.into(),
            ));
        }
        Ok(())
    }

    fn address_interval_end(
        self,
        property: &'static str,
        index: usize,
        interval: (u64, u64),
        address_bytes: usize,
    ) -> Result<u64, AddressError> {
        let (base, size) = interval;
        if size == 0 {
            return Err(self.address_error(property, Some(index), AddressErrorReason::ZeroSize));
        }
        base.checked_add(size - 1)
            .filter(|end| address_bytes == 8 || *end <= u64::from(u32::MAX))
            .ok_or_else(|| self.address_error(property, Some(index), AddressErrorReason::Overflow))
    }

    fn translate_mmio(
        mut self,
        mut address: u64,
        size: u64,
        reg_index: usize,
    ) -> Result<u64, AddressError> {
        while let Some(parent) = self.parent() {
            parent.numeric_bus()?;
            let child_bytes = self.address_cell_bytes("#address-cells", 2)?;
            let parent_bytes = parent.address_cell_bytes("#address-cells", 2)?;
            let size_bytes = self.address_cell_bytes("#size-cells", 1)?;
            let ranges = self
                .property_strict("ranges")
                .map_err(|error| self.address_error("ranges", None, error.into()))?
                .value;
            if ranges.is_empty() {
                self.address_interval_end("ranges", reg_index, (address, size), parent_bytes)?;
            } else {
                address = self.translate_mmio_ranges(
                    ranges,
                    (child_bytes, parent_bytes, size_bytes),
                    address,
                    size,
                    reg_index,
                )?;
            }
            self = parent;
        }
        Ok(address)
    }

    fn translate_mmio_ranges(
        self,
        ranges: &[u8],
        widths: (usize, usize, usize),
        address: u64,
        size: u64,
        reg_index: usize,
    ) -> Result<u64, AddressError> {
        let (child_bytes, parent_bytes, size_bytes) = widths;
        let tuple_bytes = child_bytes + parent_bytes + size_bytes;
        self.address_complete_tuples("ranges", ranges, tuple_bytes)?;
        let mut translated = None;
        for (index, tuple) in ranges.chunks_exact(tuple_bytes).enumerate() {
            let child = number(&tuple[..child_bytes]);
            let parent = number(&tuple[child_bytes..child_bytes + parent_bytes]);
            let length = number(&tuple[child_bytes + parent_bytes..]);
            self.address_interval_end("ranges", index, (child, length), child_bytes)?;
            self.address_interval_end("ranges", index, (parent, length), parent_bytes)?;
            if let Some(offset) = address.checked_sub(child)
                && size <= length
                && offset <= length - size
                && translated.is_none()
            {
                translated = parent.checked_add(offset);
            }
        }
        translated.ok_or_else(|| {
            self.address_error("ranges", Some(reg_index), AddressErrorReason::OutsideRanges)
        })
    }

    fn address_error(
        self,
        property: &'static str,
        index: Option<usize>,
        reason: AddressErrorReason,
    ) -> AddressError {
        AddressError {
            node_offset: self.offset(),
            property,
            index,
            reason,
        }
    }
}

impl From<PropertyError> for AddressErrorReason {
    fn from(error: PropertyError) -> Self {
        Self::Property(error)
    }
}

fn number(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(0, |value, byte| (value << 8) | u64::from(*byte))
}

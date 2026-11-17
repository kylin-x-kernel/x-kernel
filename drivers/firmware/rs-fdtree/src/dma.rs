// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Pure local DT DMA declarations, without host DMA policy or hardware access.

use crate::{FdtNode, PropertyError};

/// One DMA window between a bus and its immediate parent address space.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DmaRange {
    /// First address visible on the child bus.
    pub bus_addr: u64,
    /// First address in the parent bus, not necessarily a CPU physical address.
    pub parent_addr: u64,
    /// Window length in bytes.
    pub size: u64,
}

/// Invalid or unsupported local DMA property encoding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DmaRangeError {
    /// A local property is malformed or duplicated.
    Property(PropertyError),
    /// A nonempty declaration has no parent address space.
    MissingParent,
    /// Only one- and two-cell numeric addresses and sizes are supported.
    UnsupportedCells,
    /// Property length is not a whole number of range tuples.
    TruncatedTuple,
    /// A tuple describes no addresses.
    EmptyWindow,
    /// A bus or parent window extends beyond the 64-bit address space.
    AddressOverflow,
}

/// Local property state; none of these variants establishes a device DMA mask.
#[derive(Debug)]
pub enum DmaRanges<'a> {
    /// No local declaration. The caller must resolve ancestry or platform policy.
    Absent,
    /// An empty property declares identity translation at this bus boundary only.
    Identity,
    /// Explicit windows. Each iterator result must be checked before use.
    Windows(DmaRangeIter<'a>),
}

/// Allocation-free iteration over local DMA windows.
///
/// Address arithmetic is checked per tuple. Callers must reject an invalid tuple
/// rather than silently filtering it out to construct a partial DMA view.
#[derive(Debug)]
pub struct DmaRangeIter<'a> {
    tuples: core::slice::ChunksExact<'a, u8>,
    bus_bytes: usize,
    parent_bytes: usize,
}

fn number(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(0, |value, byte| (value << 8) | u64::from(*byte))
}

impl Iterator for DmaRangeIter<'_> {
    type Item = Result<DmaRange, DmaRangeError>;

    fn next(&mut self) -> Option<Self::Item> {
        let tuple = self.tuples.next()?;
        let parent_end = self.bus_bytes + self.parent_bytes;
        let range = DmaRange {
            bus_addr: number(&tuple[..self.bus_bytes]),
            parent_addr: number(&tuple[self.bus_bytes..parent_end]),
            size: number(&tuple[parent_end..]),
        };
        let Some(last_offset) = range.size.checked_sub(1) else {
            return Some(Err(DmaRangeError::EmptyWindow));
        };
        if range.bus_addr.checked_add(last_offset).is_none()
            || range.parent_addr.checked_add(last_offset).is_none()
        {
            return Some(Err(DmaRangeError::AddressOverflow));
        }
        Some(Ok(range))
    }
}

/// Decode a bus node's local `dma-ranges` property.
///
/// Supply the bus node's `#address-cells` and `#size-cells`, and its parent's
/// `#address-cells`. Widths are needed only for a nonempty property. PCI's
/// three-cell address encoding is deliberately unsupported, not truncated.
/// This function does not walk DMA parents, resolve coherency, or translate
/// parent addresses into CPU physical addresses. Missing is distinct from empty.
///
/// # Errors
///
/// Unsupported cell widths and incomplete tuples fail immediately. Zero-size
/// and overflowing windows are reported by the returned iterator.
pub fn parse_dma_ranges(
    property: Option<&[u8]>,
    bus_address_cells: u32,
    parent_address_cells: u32,
    size_cells: u32,
) -> Result<DmaRanges<'_>, DmaRangeError> {
    let Some(bytes) = property else {
        return Ok(DmaRanges::Absent);
    };
    if bytes.is_empty() {
        return Ok(DmaRanges::Identity);
    }
    if ![bus_address_cells, parent_address_cells, size_cells]
        .iter()
        .all(|cells| matches!(cells, 1 | 2))
    {
        return Err(DmaRangeError::UnsupportedCells);
    }
    let bus_bytes = bus_address_cells as usize * 4;
    let parent_bytes = parent_address_cells as usize * 4;
    let tuple_bytes = bus_bytes + parent_bytes + size_cells as usize * 4;
    if !bytes.len().is_multiple_of(tuple_bytes) {
        return Err(DmaRangeError::TruncatedTuple);
    }
    Ok(DmaRanges::Windows(DmaRangeIter {
        tuples: bytes.chunks_exact(tuple_bytes),
        bus_bytes,
        parent_bytes,
    }))
}

/// A device DMA window translated into CPU physical addresses by DT rules.
///
/// `bus_addr` is the first address in the device's bus address space and
/// `cpu_addr` the corresponding CPU physical address. The window is a
/// translation fact, not a device mask.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceDmaWindow {
    /// First address visible on the device's bus.
    pub bus_addr: u64,
    /// First CPU physical address corresponding to `bus_addr`.
    pub cpu_addr: u64,
    /// Window length in bytes.
    pub size: u64,
}

/// Why DT cannot produce a usable device DMA window.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceDmaError {
    /// A local `dma-ranges` declaration is malformed or unsupported.
    Declaration(DmaRangeError),
    /// No nonempty declaration exists on the device's DMA-parent path.
    Missing,
    /// The DMA-parent path declares more than one window.
    MultiWindow,
    /// `interconnects`/`interconnect-names` redirect the DMA parent.
    UnsupportedParent,
    /// `iommus`/`iommu-map`/`iommu-map-mask` control this device's addresses.
    Iommu,
    /// Parent addresses cannot be translated to CPU physical addresses.
    UnsupportedTranslation,
    /// The window is empty, ambiguous or overflows an address space.
    InvalidWindow,
}

impl From<DmaRangeError> for DeviceDmaError {
    fn from(error: DmaRangeError) -> Self {
        Self::Declaration(error)
    }
}

impl<'b, 'a: 'b> FdtNode<'b, 'a> {
    /// Decode all local DMA windows without translating structural ancestry.
    ///
    /// Missing and empty properties remain distinct. Nonempty tuples use the
    /// local bus widths and the structural parent's address width, with DT's
    /// local defaults of two address cells and one size cell. No host mask,
    /// coherent default, single-window restriction or IOMMU policy is applied.
    pub fn dma_ranges(self) -> Result<DmaRanges<'a>, DmaRangeError> {
        let property = match self.property_strict("dma-ranges") {
            Ok(property) => property.value,
            Err(PropertyError::Missing) => return Ok(DmaRanges::Absent),
            Err(error) => return Err(DmaRangeError::Property(error)),
        };
        if property.is_empty() {
            return Ok(DmaRanges::Identity);
        }
        let parent = self.parent().ok_or(DmaRangeError::MissingParent)?;
        parse_dma_ranges(
            Some(property),
            dma_cells(self, "#address-cells", 2)?,
            dma_cells(parent, "#address-cells", 2)?,
            dma_cells(self, "#size-cells", 1)?,
        )
    }

    /// Resolve this device's DMA window using DT's DMA-parent rules.
    ///
    /// Mirrors the OF DMA walk: start at the structural parent, skip empty
    /// declarations while the walk continues, and stop at the first nonempty
    /// declaration. Its parent-side address is translated across every
    /// ancestor boundary until the root address space, which is the CPU
    /// address space. Only one window is representable; interconnect overrides
    /// and IOMMU-controlled addresses are reported as unsupported instead of
    /// being guessed. A missing declaration on the first node is tolerated but
    /// a later absence is not, matching Linux's `of_dma_get_range()`.
    ///
    /// # Errors
    ///
    /// Returns [`DeviceDmaError`] for malformed declarations, multi-window,
    /// IOMMU or interconnect topology and untranslatable windows.
    pub fn device_dma_window(self) -> Result<Option<DeviceDmaWindow>, DeviceDmaError> {
        reject_unsupported_dma_topology(self)?;
        let mut node = self.parent().ok_or(DeviceDmaError::Missing)?;
        let mut first = true;
        loop {
            match node.dma_ranges()? {
                DmaRanges::Absent if !first => return Err(DeviceDmaError::Missing),
                DmaRanges::Absent | DmaRanges::Identity => {}
                DmaRanges::Windows(windows) => {
                    let mut selected = None;
                    for range in windows {
                        let range = range?;
                        if selected.is_some() {
                            return Err(DeviceDmaError::MultiWindow);
                        }
                        selected = Some(range);
                    }
                    let range = selected.ok_or(DeviceDmaError::InvalidWindow)?;
                    let cpu_addr = translate_to_cpu(node, range.parent_addr, range.size)?;
                    return Ok(Some(DeviceDmaWindow {
                        bus_addr: range.bus_addr,
                        cpu_addr,
                        size: range.size,
                    }));
                }
            }
            node = node.parent().ok_or(DeviceDmaError::Missing)?;
            first = false;
        }
    }

    /// Resolve the nearest explicit coherency declaration on this node's
    /// ancestry.
    ///
    /// The device node is consulted first. `Ok(None)` means no ancestor
    /// declares anything; it is not a platform default.
    ///
    /// # Errors
    ///
    /// Returns the declaration error for a malformed or contradictory flag.
    pub fn inherited_dma_coherency(self) -> Result<Option<bool>, PropertyError> {
        let mut node = Some(self);
        while let Some(current) = node {
            match current.dma_coherency()? {
                Some(coherent) => return Ok(Some(coherent)),
                None => node = current.parent(),
            }
        }
        Ok(None)
    }

    /// Read only this node's coherency declaration, without inheritance/defaults.
    ///
    /// Boolean declarations must be empty and mutually exclusive. Duplicate
    /// properties and malformed flags return typed errors, never a host policy.
    pub fn dma_coherency(self) -> Result<Option<bool>, PropertyError> {
        let flag = |name| match self.property_strict(name) {
            Ok(property) if property.value.is_empty() => Ok(true),
            Ok(_) => Err(PropertyError::InvalidEncoding),
            Err(PropertyError::Missing) => Ok(false),
            Err(error) => Err(error),
        };
        match (flag("dma-coherent")?, flag("dma-noncoherent")?) {
            (true, true) => Err(PropertyError::InvalidEncoding),
            (true, false) => Ok(Some(true)),
            (false, true) => Ok(Some(false)),
            (false, false) => Ok(None),
        }
    }
}

/// Refuse topology this window cannot represent instead of guessing.
///
/// `interconnect-names` may redirect the DMA parent away from the structural
/// parent, and `iommus`/`iommu-map` make device addresses IOMMU addresses.
fn reject_unsupported_dma_topology(device: FdtNode<'_, '_>) -> Result<(), DeviceDmaError> {
    let mut node = Some(device);
    while let Some(current) = node {
        if ["interconnects", "interconnect-names"]
            .iter()
            .any(|name| current.property(name).is_some())
        {
            return Err(DeviceDmaError::UnsupportedParent);
        }
        if ["iommus", "iommu-map", "iommu-map-mask"]
            .iter()
            .any(|name| current.property(name).is_some())
        {
            return Err(DeviceDmaError::Iommu);
        }
        node = current.parent();
    }
    Ok(())
}

/// Translate a declaration's parent-side start into CPU physical space.
///
/// The declaration node's own window is not reapplied: its parent address is
/// already expressed in the immediate parent's address space. Each ancestor
/// boundary is crossed with that ancestor's local `dma-ranges`, where an
/// absent or empty declaration is identity. The root address space is the CPU
/// address space.
fn translate_to_cpu(
    declaration: FdtNode<'_, '_>,
    parent_addr: u64,
    size: u64,
) -> Result<u64, DeviceDmaError> {
    if size == 0 {
        return Err(DeviceDmaError::InvalidWindow);
    }
    let Some(mut node) = declaration.parent() else {
        // Without a parent there is no boundary to translate across; refusing
        // is safer than silently treating an arbitrary bus address as CPU.
        return Err(DeviceDmaError::UnsupportedTranslation);
    };
    let mut address = parent_addr;
    while let Some(parent) = node.parent() {
        address = translate_one(node, address, size)?;
        node = parent;
    }
    Ok(address)
}

fn translate_one(node: FdtNode<'_, '_>, address: u64, size: u64) -> Result<u64, DeviceDmaError> {
    match node.dma_ranges()? {
        DmaRanges::Absent | DmaRanges::Identity => Ok(address),
        DmaRanges::Windows(windows) => {
            let mut translated = None;
            for range in windows {
                let range = range?;
                let Some(offset) = window_offset(range.bus_addr, range.size, address, size) else {
                    continue;
                };
                if translated.is_some() {
                    // Overlapping ancestor windows make the mapping ambiguous.
                    return Err(DeviceDmaError::UnsupportedTranslation);
                }
                translated = Some(
                    range
                        .parent_addr
                        .checked_add(offset)
                        .ok_or(DeviceDmaError::InvalidWindow)?,
                );
            }
            translated.ok_or(DeviceDmaError::InvalidWindow)
        }
    }
}

/// Offset of `[address, address + size)` inside a window, if it is contained.
fn window_offset(start: u64, length: u64, address: u64, size: u64) -> Option<u64> {
    if size == 0 {
        return None;
    }
    let offset = address.checked_sub(start)?;
    let last_offset = length.checked_sub(1)?;
    if size - 1 > last_offset.checked_sub(offset)? {
        return None;
    }
    Some(offset)
}

fn dma_cells(node: FdtNode<'_, '_>, name: &str, default: u32) -> Result<u32, DmaRangeError> {
    match node.read_u32(name) {
        Ok(cells) => Ok(cells),
        Err(PropertyError::Missing) => Ok(default),
        Err(error) => Err(DmaRangeError::Property(error)),
    }
}

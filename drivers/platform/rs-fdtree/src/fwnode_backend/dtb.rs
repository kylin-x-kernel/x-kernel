// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device-tree backend for the neutral firmware-node contract.
//!
//! The generic device-tree interpretation — node structure, properties,
//! availability, identity strings, phandle references, interrupt inheritance
//! and translated MMIO resources — lives in this crate's core API. This
//! module only translates those borrowed results into [`fwnode`] handles and
//! fixed-capacity parameters. Binding-specific meaning of interrupt cells
//! remains with the controller driver.

use ::fwnode::{
    FW_IRQ_SPEC_MAX_PARAMS, FwDmaError, FwDmaRange, FwDmaRanges, FwDmaWindow, FwError, FwIrqSpec,
    FwMmioResource, FwNode, FwNodeId, FwNodeOps, FwParams, FwProperty, FwPropertyValue,
    FwReference, FwSource,
};
use lazyinit::LazyInit;

use crate::{
    AddressError, AddressErrorReason, Cells, DeviceDmaError, DmaRangeError, DmaRanges, FdtNode,
    LinuxFdt, NodeIndex, PropertyError, ReferenceError,
};

/// A borrowed device tree implementing [`FwNodeOps`].
///
/// The node index is built on first use, so publishing the backend during
/// early boot stays allocation-free; callers reach indexed lookups only once
/// the kernel allocator is available.
pub struct DtBackend<'a> {
    tree: LinuxFdt<'a>,
    index: LazyInit<NodeIndex<'a>>,
}

impl<'a> DtBackend<'a> {
    /// Wraps a validated device tree as a firmware-node backend.
    pub const fn new(tree: LinuxFdt<'a>) -> Self {
        Self {
            tree,
            index: LazyInit::new(),
        }
    }

    /// Returns the wrapped device tree.
    pub fn tree(&self) -> LinuxFdt<'a> {
        self.tree
    }

    fn index(&self) -> &NodeIndex<'a> {
        loop {
            if let Some(index) = self.index.get() {
                return index;
            }
            if let Some(index) = self.index.call_once(|| NodeIndex::new(self.tree)) {
                return index;
            }
        }
    }

    /// Returns a handle to the root node.
    ///
    /// The parser guarantees a root node, but this returns `None` rather than
    /// panicking if the tree is queried after invalid construction.
    pub fn root<'s>(&'s self) -> Option<FwNode<'s>>
    where
        'a: 's,
    {
        let node = self.tree.find_node("/")?;
        Some(self.fwnode(node))
    }

    fn node(&self, id: FwNodeId) -> Option<FdtNode<'_, 'a>> {
        self.index().node_by_offset(id.raw())
    }

    fn fwnode<'s>(&'s self, node: FdtNode<'s, 'a>) -> FwNode<'s>
    where
        'a: 's,
    {
        FwNode::new(self, FwNodeId::new(node.offset()))
    }
}

impl<'s, 'a: 's> FwNodeOps<'s> for DtBackend<'a> {
    fn source(&self) -> FwSource {
        FwSource::DeviceTree
    }

    fn name(&self, id: FwNodeId) -> Option<&'s str> {
        Some(self.node(id)?.name)
    }

    fn parent(&self, id: FwNodeId) -> Option<FwNodeId> {
        Some(FwNodeId::new(self.node(id)?.parent()?.offset()))
    }

    fn first_child(&self, id: FwNodeId) -> Option<FwNodeId> {
        Some(FwNodeId::new(self.node(id)?.first_child()?.offset()))
    }

    fn next_sibling(&self, id: FwNodeId) -> Option<FwNodeId> {
        Some(FwNodeId::new(self.node(id)?.next_sibling()?.offset()))
    }

    fn is_available(&self, id: FwNodeId) -> Result<bool, FwError> {
        self.node(id)
            .ok_or(FwError::InvalidNode)?
            .is_available()
            .map_err(|_| FwError::InvalidValue)
    }

    fn is_interrupt_controller(&self, id: FwNodeId) -> Result<bool, FwError> {
        let node = self.node(id).ok_or(FwError::InvalidNode)?;
        Ok(node.property("interrupt-controller").is_some())
    }

    fn firmware_id(&self, id: FwNodeId, index: usize) -> Option<&'s str> {
        self.node(id)?.compatibles().nth(index)
    }

    fn property(&self, id: FwNodeId, name: &str) -> Result<Option<FwProperty<'s>>, FwError> {
        let node = self.node(id).ok_or(FwError::InvalidNode)?;
        Ok(node.property(name).map(|property| FwProperty {
            name: property.name,
            value: FwPropertyValue::Bytes(property.value),
        }))
    }

    fn mmio_resource(
        &self,
        id: FwNodeId,
        index: usize,
    ) -> Result<Option<FwMmioResource<'s>>, FwError> {
        let node = self.node(id).ok_or(FwError::InvalidNode)?;
        let resources = node.mmio_resources().map_err(address_error)?;
        Ok(resources.get(index).map(|resource| FwMmioResource {
            base: resource.base,
            size: resource.size,
            name: resource.name,
        }))
    }

    fn dma_ranges(&self, id: FwNodeId) -> Result<FwDmaRanges, FwError> {
        let node = self.node(id).ok_or(FwError::InvalidNode)?;
        match node.dma_ranges().map_err(dma_error)? {
            DmaRanges::Absent => Ok(FwDmaRanges::Absent),
            DmaRanges::Identity => Ok(FwDmaRanges::Identity),
            DmaRanges::Windows(windows) => {
                let mut count = 0;
                for range in windows {
                    range.map_err(dma_error)?;
                    count += 1;
                }
                Ok(FwDmaRanges::Windows(count))
            }
        }
    }

    fn dma_range(&self, id: FwNodeId, index: usize) -> Result<Option<FwDmaRange>, FwError> {
        let node = self.node(id).ok_or(FwError::InvalidNode)?;
        let DmaRanges::Windows(windows) = node.dma_ranges().map_err(dma_error)? else {
            return Ok(None);
        };
        let mut selected = None;
        for (current, range) in windows.enumerate() {
            let range = range.map_err(dma_error)?;
            if current == index {
                selected = Some(FwDmaRange {
                    bus_addr: range.bus_addr,
                    parent_addr: range.parent_addr,
                    size: range.size,
                });
            }
        }
        Ok(selected)
    }

    fn dma_coherency(&self, id: FwNodeId) -> Result<Option<bool>, FwError> {
        self.node(id)
            .ok_or(FwError::InvalidNode)?
            .dma_coherency()
            .map_err(property_error)
    }

    fn dma_window(&self, id: FwNodeId) -> Result<Option<FwDmaWindow>, FwDmaError> {
        let node = self
            .node(id)
            .ok_or(FwDmaError::Firmware(FwError::InvalidNode))?;
        match node.device_dma_window() {
            Ok(window) => Ok(window.map(|window| FwDmaWindow {
                bus_addr: window.bus_addr,
                cpu_addr: window.cpu_addr,
                size: window.size,
            })),
            // Device tree keeps "no nonempty declaration on the DMA-parent
            // path" distinct internally, but the neutral contract represents
            // absence as `Ok(None)`, never as an error. Only malformed or
            // unsupported descriptions remain errors here.
            Err(DeviceDmaError::Missing) => Ok(None),
            Err(DeviceDmaError::Declaration(error)) => Err(FwDmaError::Firmware(dma_error(error))),
            Err(DeviceDmaError::MultiWindow) => Err(FwDmaError::MultiWindow),
            Err(DeviceDmaError::UnsupportedParent) => Err(FwDmaError::UnsupportedParent),
            Err(DeviceDmaError::Iommu) => Err(FwDmaError::Iommu),
            Err(DeviceDmaError::UnsupportedTranslation) => Err(FwDmaError::UnsupportedTranslation),
            Err(DeviceDmaError::InvalidWindow) => Err(FwDmaError::InvalidWindow),
        }
    }

    fn dma_effective_coherency(&self, id: FwNodeId) -> Result<Option<bool>, FwError> {
        self.node(id)
            .ok_or(FwError::InvalidNode)?
            .inherited_dma_coherency()
            .map_err(property_error)
    }

    fn irq_spec(&'s self, id: FwNodeId, index: usize) -> Result<Option<FwIrqSpec<'s>>, FwError> {
        let node = self.node(id).ok_or(FwError::InvalidNode)?;
        let Some(spec) = node
            .irq_spec_with(self.index(), index)
            .map_err(reference_error)?
        else {
            return Ok(None);
        };
        Ok(Some(FwIrqSpec {
            controller: self.fwnode(spec.controller),
            params: params(spec.params)?,
        }))
    }

    fn reference(
        &'s self,
        id: FwNodeId,
        property: &str,
        index: usize,
        arg_cells: usize,
    ) -> Result<Option<FwReference<'s>>, FwError> {
        let node = self.node(id).ok_or(FwError::InvalidNode)?;
        if arg_cells > FW_IRQ_SPEC_MAX_PARAMS {
            return Err(FwError::BufferTooSmall);
        }
        let Some(reference) = node
            .reference_with(self.index(), property, index, arg_cells)
            .map_err(reference_error)?
        else {
            return Ok(None);
        };
        Ok(Some(FwReference {
            node: self.fwnode(reference.node),
            args: params(reference.args)?,
        }))
    }
}

fn params(cells: Cells<'_>) -> Result<FwParams, FwError> {
    let mut values = [0u32; FW_IRQ_SPEC_MAX_PARAMS];
    cells
        .read_into(&mut values)
        .map_err(|_| FwError::BufferTooSmall)?;
    FwParams::new(&values[..cells.len()]).map_err(|_| FwError::BufferTooSmall)
}

fn reference_error(error: ReferenceError) -> FwError {
    match error {
        ReferenceError::Property(PropertyError::BufferTooSmall { .. }) => FwError::BufferTooSmall,
        _ => FwError::InvalidValue,
    }
}

fn address_error(error: AddressError) -> FwError {
    match error.reason {
        AddressErrorReason::Property(PropertyError::Missing) => FwError::Missing,
        AddressErrorReason::UnsupportedBus | AddressErrorReason::UnsupportedCells(_) => {
            FwError::Unsupported
        }
        _ => FwError::InvalidValue,
    }
}

fn dma_error(error: DmaRangeError) -> FwError {
    match error {
        DmaRangeError::Property(PropertyError::Missing) => FwError::Missing,
        DmaRangeError::UnsupportedCells => FwError::Unsupported,
        _ => FwError::InvalidValue,
    }
}

fn property_error(error: PropertyError) -> FwError {
    match error {
        PropertyError::Missing => FwError::Missing,
        PropertyError::BufferTooSmall { .. } => FwError::BufferTooSmall,
        _ => FwError::InvalidValue,
    }
}

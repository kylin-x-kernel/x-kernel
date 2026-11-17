// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Neutral firmware-node contract.
//!
//! Device description comes from different firmware interfaces: a flattened
//! device tree, ACPI namespace objects (or MADT-derived nodes), and static
//! board tables. This crate defines the small, allocation-free interface that
//! those backends implement and that consumers such as interrupt-controller
//! drivers use. It deliberately contains no firmware parser and no kernel
//! mapping policy.
//!
//! A [`FwNode`] is a copyable handle: a backend-local [`FwNodeId`] plus a
//! reference to the backend operations. Handles remain valid while the
//! backend lives. Tree traversal is index-based so the trait stays
//! object-safe; convenience iterators on [`FwNode`] build on that.
//!
//! The contract describes facts, not Linux `irq_domain` behavior. In
//! particular, [`FwIrqSpec`] carries a controller handle and its raw cell
//! arguments; interpretation and mapping belong to the controller driver.

#![no_std]

use core::fmt;

/// Maximum number of cells carried by one parameter list.
///
/// This matches Linux's `IRQ_DOMAIN_IRQ_SPEC_PARAMS` so interrupt specifiers
/// (and similarly sized reference arguments) can be consumed without a heap
/// allocation.
pub const FW_IRQ_SPEC_MAX_PARAMS: usize = 16;

/// A backend-local node identity.
///
/// The value is opaque to consumers. It is only meaningful together with the
/// [`FwNodeOps`] instance that produced it; in particular, a device-tree
/// offset is not a firmware-global identity.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FwNodeId(u32);

impl FwNodeId {
    /// Creates an identity encoded by a backend.
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// Returns the backend-local encoding.
    ///
    /// Consumers must not interpret the value; use it only for diagnostics or
    /// to hand it back to the same backend.
    pub const fn raw(self) -> u32 {
        self.0
    }
}

/// A firmware-description access failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FwError {
    /// The handle does not identify a node in this backend.
    InvalidNode,
    /// A required property or resource is absent.
    Missing,
    /// The firmware value cannot be interpreted as requested.
    InvalidValue,
    /// The backend or the binding is not supported yet.
    Unsupported,
    /// A fixed-capacity destination is too small.
    BufferTooSmall,
}

/// The firmware interface that produced a node description.
///
/// This is descriptive metadata, not another identity: the node is still
/// identified by [`FwNode`] alone. Consumers use it for diagnostics and for
/// selecting the correct resource provenance without matching compatible
/// strings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FwSource {
    /// Flattened device tree.
    DeviceTree,
    /// ACPI namespace or ACPI-derived description.
    Acpi,
    /// Static board or platform table.
    Static,
    /// The backend does not report its provenance.
    Unknown,
}

/// Operations implemented by one device-description backend.
///
/// Implementations are shared immutably. Methods return borrowed firmware
/// data, so the backend must outlive every [`FwNode`] created from it.
pub trait FwNodeOps<'a>: Sync + 'a {
    /// Returns the firmware interface that produced this backend.
    ///
    /// The default keeps unrelated test backends source-agnostic; concrete
    /// firmware backends should report their real provenance.
    fn source(&self) -> FwSource {
        FwSource::Unknown
    }

    /// Returns the node name, or `None` for an invalid handle.
    fn name(&self, id: FwNodeId) -> Option<&'a str>;

    /// Returns the structural parent, or `None` for the root or an invalid
    /// handle.
    fn parent(&self, id: FwNodeId) -> Option<FwNodeId>;

    /// Returns the first direct child, or `None` when there is none.
    fn first_child(&self, id: FwNodeId) -> Option<FwNodeId>;

    /// Returns the next direct sibling, or `None` when there is none.
    fn next_sibling(&self, id: FwNodeId) -> Option<FwNodeId>;

    /// Returns whether the node is available to the OS.
    ///
    /// Backends must distinguish "available" from "cannot decide": a
    /// malformed availability property is an error, not `false`.
    ///
    /// # Errors
    ///
    /// Returns [`FwError::InvalidNode`] for an unknown handle and
    /// [`FwError::InvalidValue`] when the availability declaration is
    /// malformed.
    fn is_available(&self, id: FwNodeId) -> Result<bool, FwError>;

    /// Returns whether firmware declares this node as an interrupt controller.
    ///
    /// The backend owns how its firmware represents this role; callers must
    /// not test a device-tree property name themselves. A missing declaration
    /// is `Ok(false)`, while a malformed declaration is an error.
    ///
    /// # Errors
    ///
    /// Returns [`FwError::InvalidNode`] for an unknown handle or a backend
    /// error when the declaration cannot be interpreted.
    fn is_interrupt_controller(&self, id: FwNodeId) -> Result<bool, FwError>;

    /// Returns firmware identity string `index`, in firmware order.
    ///
    /// For device tree this is the `compatible` list; other backends return
    /// their closest stable equivalent (for example `_HID`/`_CID`).
    fn firmware_id(&self, id: FwNodeId, index: usize) -> Option<&'a str>;

    /// Returns a named property.
    ///
    /// A missing property is `Ok(None)`. Values stay in firmware
    /// representation; use [`FwProperty`] accessors to interpret them.
    ///
    /// # Errors
    ///
    /// Returns [`FwError::InvalidNode`] for an unknown handle, or a backend
    /// error if the node's property stream is malformed.
    fn property(&self, id: FwNodeId, name: &str) -> Result<Option<FwProperty<'a>>, FwError>;

    /// Returns this node's firmware-declared hardware CPU identity.
    ///
    /// The identity is an architecture-defined hardware CPU number, not a
    /// logical CPU index, firmware-node identity or ACPI Processor UID. For
    /// example, a device-tree RISC-V CPU node reports its hart ID here.
    /// Consumers must not use this value as a `RawCpuId` without the owning
    /// architecture's mapping and validation.
    ///
    /// `Some(id)` is a successfully decoded identity; zero is valid. `None`
    /// means the node is not a processor node or declares no identity.
    /// Malformed declarations return [`FwError::InvalidValue`]. Backends that
    /// do not implement this capability return [`FwError::Unsupported`].
    ///
    /// # Errors
    ///
    /// Returns [`FwError::InvalidNode`] for an unknown handle,
    /// [`FwError::InvalidValue`] for a malformed identity declaration, or
    /// [`FwError::Unsupported`] when the backend does not provide this fact.
    fn hardware_cpu_id(&self, id: FwNodeId) -> Result<Option<u64>, FwError> {
        let _ = id;
        Err(FwError::Unsupported)
    }

    /// Returns MMIO resource `index` in firmware order.
    ///
    /// An out-of-range index is `Ok(None)`, which lets callers enumerate
    /// resources without a separate count.
    ///
    /// # Errors
    ///
    /// Returns [`FwError::InvalidNode`] for an unknown handle,
    /// [`FwError::Unsupported`] for an address encoding this backend cannot
    /// translate, or [`FwError::InvalidValue`] for malformed resources.
    fn mmio_resource(
        &self,
        id: FwNodeId,
        index: usize,
    ) -> Result<Option<FwMmioResource<'a>>, FwError>;

    /// Returns the node's local DMA range declaration.
    ///
    /// The declaration is local to this node's bus boundary. A backend must
    /// keep missing and empty declarations distinct, and must not apply
    /// ancestor translation, DMA masks, coherent defaults or platform policy.
    /// Backends without DMA facts return [`FwError::Unsupported`].
    ///
    /// # Errors
    ///
    /// Returns [`FwError::InvalidNode`] for an unknown handle,
    /// [`FwError::Unsupported`] for an encoding this backend cannot interpret,
    /// or [`FwError::InvalidValue`] for a malformed declaration.
    fn dma_ranges(&self, id: FwNodeId) -> Result<FwDmaRanges, FwError> {
        let _ = id;
        Err(FwError::Unsupported)
    }

    /// Returns local DMA window `index`, or `None` when there is no such
    /// window.
    ///
    /// Callers should use [`FwNodeOps::dma_ranges`] to distinguish absence,
    /// identity and explicit windows. This accessor validates the complete
    /// declaration, so a malformed later tuple cannot be hidden by reading an
    /// earlier one.
    ///
    /// # Errors
    ///
    /// Returns [`FwError::InvalidNode`] for an unknown handle,
    /// [`FwError::Unsupported`] for an encoding this backend cannot interpret,
    /// or [`FwError::InvalidValue`] for a malformed declaration.
    fn dma_range(&self, id: FwNodeId, index: usize) -> Result<Option<FwDmaRange>, FwError> {
        let _ = (id, index);
        Err(FwError::Unsupported)
    }

    /// Returns this node's local coherency declaration.
    ///
    /// `Some(true)` and `Some(false)` are explicit firmware declarations.
    /// `None` means this node declares neither; it is not the platform default
    /// and must not be interpreted as coherent. Backends without coherency
    /// facts return [`FwError::Unsupported`].
    ///
    /// # Errors
    ///
    /// Returns [`FwError::InvalidNode`] for an unknown handle or
    /// [`FwError::InvalidValue`] when the declaration is malformed or
    /// contradictory.
    fn dma_coherency(&self, id: FwNodeId) -> Result<Option<bool>, FwError> {
        let _ = id;
        Err(FwError::Unsupported)
    }

    /// Resolves the device DMA window using this firmware's parent and
    /// translation rules.
    ///
    /// The backend owns the firmware-specific algorithm: which ancestor
    /// declares the DMA parent, how an empty or absent declaration is treated,
    /// how the parent-side address is translated across ancestor boundaries,
    /// and which firmware properties make a translation unsupported. The
    /// result is already expressed in CPU physical addresses and remains a
    /// firmware fact: it is neither a device mask nor a permission to DMA.
    ///
    /// `Ok(None)` means firmware describes no usable window, including the
    /// case where no nonempty declaration exists on the device's DMA-parent
    /// path. Absence is never an error; backends without DMA facts return
    /// [`FwDmaError::Firmware`] wrapping [`FwError::Unsupported`].
    ///
    /// # Errors
    ///
    /// Returns [`FwDmaError`] for malformed declarations, unsupported topology
    /// (multi-window, interconnect override, IOMMU) and untranslatable
    /// windows. A backend must report these explicitly rather than selecting a
    /// window, treating an absent declaration as identity, or turning absence
    /// into an error.
    fn dma_window(&self, id: FwNodeId) -> Result<Option<FwDmaWindow>, FwDmaError> {
        let _ = id;
        Err(FwDmaError::Firmware(FwError::Unsupported))
    }

    /// Resolves the effective coherency declaration by firmware inheritance.
    ///
    /// The backend applies this firmware's ancestry rule (for example, the
    /// nearest explicit declaration on the device's parent chain) and returns
    /// `Ok(None)` when no ancestor declares anything. No platform default is
    /// applied here.
    ///
    /// # Errors
    ///
    /// Returns [`FwError::InvalidNode`] for an unknown handle or a backend
    /// error when a declaration cannot be interpreted.
    fn dma_effective_coherency(&self, id: FwNodeId) -> Result<Option<bool>, FwError> {
        let _ = id;
        Err(FwError::Unsupported)
    }

    /// Returns interrupt specifier `index` for this node.
    ///
    /// The returned controller handle and parameter cells are raw firmware
    /// facts. A controller driver interprets them according to its binding.
    /// The receiver is borrowed for `'a` so the controller handle can be
    /// stored in the result.
    ///
    /// # Errors
    ///
    /// Returns [`FwError::InvalidNode`] for an unknown handle,
    /// [`FwError::InvalidValue`] for a malformed specifier,
    /// [`FwError::BufferTooSmall`] when the specifier does not fit
    /// [`FW_IRQ_SPEC_MAX_PARAMS`], or [`FwError::Unsupported`] when this
    /// backend cannot resolve it yet.
    fn irq_spec(&'a self, id: FwNodeId, index: usize) -> Result<Option<FwIrqSpec<'a>>, FwError>;

    /// Resolves reference `index` of `property`.
    ///
    /// A reference is a target node followed by `arg_cells` argument cells.
    /// The argument count is defined by the referenced binding (for example
    /// `#clock-cells`), not by this contract; pass zero for bare phandles.
    /// An out-of-range index is `Ok(None)`.
    ///
    /// The receiver is borrowed for `'a` so the target handle can be stored
    /// in the result.
    ///
    /// # Errors
    ///
    /// Returns [`FwError::InvalidNode`] for an unknown handle,
    /// [`FwError::InvalidValue`] for a malformed or unresolvable reference,
    /// or [`FwError::BufferTooSmall`] when `arg_cells` exceeds
    /// [`FW_IRQ_SPEC_MAX_PARAMS`].
    fn reference(
        &'a self,
        id: FwNodeId,
        property: &str,
        index: usize,
        arg_cells: usize,
    ) -> Result<Option<FwReference<'a>>, FwError>;
}

/// A copyable handle to one firmware node.
#[derive(Clone, Copy)]
pub struct FwNode<'a> {
    ops: &'a dyn FwNodeOps<'a>,
    id: FwNodeId,
}

impl fmt::Debug for FwNode<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FwNode")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl PartialEq for FwNode<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && core::ptr::eq(self.ops, other.ops)
    }
}

impl Eq for FwNode<'_> {}

impl<'a> FwNode<'a> {
    /// Creates a handle from a backend and its node identity.
    pub const fn new(ops: &'a dyn FwNodeOps<'a>, id: FwNodeId) -> Self {
        Self { ops, id }
    }

    /// Returns the backend-local identity.
    pub const fn id(self) -> FwNodeId {
        self.id
    }

    /// Returns the firmware interface that produced this node.
    pub fn source(self) -> FwSource {
        self.ops.source()
    }

    /// Returns the node name.
    pub fn name(self) -> Option<&'a str> {
        self.ops.name(self.id)
    }

    /// Returns the structural parent.
    pub fn parent(self) -> Option<Self> {
        self.ops.parent(self.id).map(|id| Self::new(self.ops, id))
    }

    /// Returns the first direct child.
    pub fn first_child(self) -> Option<Self> {
        self.ops
            .first_child(self.id)
            .map(|id| Self::new(self.ops, id))
    }

    /// Returns the next direct sibling.
    pub fn next_sibling(self) -> Option<Self> {
        self.ops
            .next_sibling(self.id)
            .map(|id| Self::new(self.ops, id))
    }

    /// Returns the next node in depth-first pre-order, or `None` at the end
    /// of the subtree containing this node.
    pub fn next_dfs(self) -> Option<Self> {
        if let Some(child) = self.first_child() {
            return Some(child);
        }
        let mut node = self;
        loop {
            if let Some(sibling) = node.next_sibling() {
                return Some(sibling);
            }
            node = node.parent()?;
        }
    }

    /// Iterates this node's direct children.
    pub fn children(self) -> impl Iterator<Item = Self> + 'a {
        let mut next = self.first_child();
        core::iter::from_fn(move || {
            let node = next?;
            next = node.next_sibling();
            Some(node)
        })
    }

    /// Iterates this node and every descendant in depth-first pre-order.
    pub fn descendants(self) -> impl Iterator<Item = Self> + 'a {
        core::iter::successors(Some(self), |node| node.next_dfs())
    }

    /// Returns whether the node is available to the OS.
    ///
    /// # Errors
    ///
    /// See [`FwNodeOps::is_available`].
    pub fn is_available(self) -> Result<bool, FwError> {
        self.ops.is_available(self.id)
    }

    /// Returns whether firmware declares this node as an interrupt controller.
    ///
    /// # Errors
    ///
    /// See [`FwNodeOps::is_interrupt_controller`].
    pub fn is_interrupt_controller(self) -> Result<bool, FwError> {
        self.ops.is_interrupt_controller(self.id)
    }

    /// Iterates firmware identity strings in firmware order.
    pub fn firmware_ids(self) -> impl Iterator<Item = &'a str> + 'a {
        let mut index = 0usize;
        core::iter::from_fn(move || {
            let id = self.ops.firmware_id(self.id, index)?;
            index += 1;
            Some(id)
        })
    }

    /// Returns a named property.
    ///
    /// # Errors
    ///
    /// See [`FwNodeOps::property`].
    pub fn property(self, name: &str) -> Result<Option<FwProperty<'a>>, FwError> {
        self.ops.property(self.id, name)
    }

    /// Returns this node's firmware-declared hardware CPU identity.
    ///
    /// This is an architecture-defined hardware CPU number; it is not a
    /// logical CPU index, node identity or ACPI Processor UID.
    ///
    /// # Errors
    ///
    /// See [`FwNodeOps::hardware_cpu_id`].
    pub fn hardware_cpu_id(self) -> Result<Option<u64>, FwError> {
        self.ops.hardware_cpu_id(self.id)
    }

    /// Returns MMIO resource `index`.
    ///
    /// # Errors
    ///
    /// See [`FwNodeOps::mmio_resource`].
    pub fn mmio_resource(self, index: usize) -> Result<Option<FwMmioResource<'a>>, FwError> {
        self.ops.mmio_resource(self.id, index)
    }

    /// Returns the node's local DMA range declaration.
    ///
    /// # Errors
    ///
    /// See [`FwNodeOps::dma_ranges`].
    pub fn dma_ranges(self) -> Result<FwDmaRanges, FwError> {
        self.ops.dma_ranges(self.id)
    }

    /// Returns local DMA window `index`.
    ///
    /// # Errors
    ///
    /// See [`FwNodeOps::dma_range`].
    pub fn dma_range(self, index: usize) -> Result<Option<FwDmaRange>, FwError> {
        self.ops.dma_range(self.id, index)
    }

    /// Returns the node's local coherency declaration.
    ///
    /// # Errors
    ///
    /// See [`FwNodeOps::dma_coherency`].
    pub fn dma_coherency(self) -> Result<Option<bool>, FwError> {
        self.ops.dma_coherency(self.id)
    }

    /// Resolves the device DMA window through firmware's own rules.
    ///
    /// # Errors
    ///
    /// See [`FwNodeOps::dma_window`].
    pub fn dma_window(self) -> Result<Option<FwDmaWindow>, FwDmaError> {
        self.ops.dma_window(self.id)
    }

    /// Resolves the effective coherency declaration through firmware's own
    /// inheritance rule.
    ///
    /// # Errors
    ///
    /// See [`FwNodeOps::dma_effective_coherency`].
    pub fn dma_effective_coherency(self) -> Result<Option<bool>, FwError> {
        self.ops.dma_effective_coherency(self.id)
    }

    /// Returns interrupt specifier `index`.
    ///
    /// # Errors
    ///
    /// See [`FwNodeOps::irq_spec`].
    pub fn irq_spec(self, index: usize) -> Result<Option<FwIrqSpec<'a>>, FwError> {
        self.ops.irq_spec(self.id, index)
    }

    /// Resolves reference `index` of `property`.
    ///
    /// # Errors
    ///
    /// See [`FwNodeOps::reference`].
    pub fn reference(
        self,
        property: &str,
        index: usize,
        arg_cells: usize,
    ) -> Result<Option<FwReference<'a>>, FwError> {
        self.ops.reference(self.id, property, index, arg_cells)
    }
}

/// One firmware property value in its natural representation.
///
/// Backends that can synthesize typed values use the matching variant;
/// device-tree properties normally use [`FwPropertyValue::Bytes`] and are
/// interpreted on demand.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FwPropertyValue<'a> {
    /// A flag property with no payload (for example `interrupt-controller`).
    Empty,
    /// Raw firmware bytes, for example a device-tree property payload.
    Bytes(&'a [u8]),
    /// A 32-bit integer supplied by a non-device-tree backend.
    U32(u32),
    /// A 64-bit integer supplied by a non-device-tree backend.
    U64(u64),
    /// A boolean supplied by a non-device-tree backend.
    Bool(bool),
    /// A string supplied by a non-device-tree backend.
    Str(&'a str),
}

/// A named firmware property.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FwProperty<'a> {
    /// Property name.
    pub name: &'a str,
    /// Property value.
    pub value: FwPropertyValue<'a>,
}

impl<'a> FwProperty<'a> {
    /// Interprets the value as a big-endian 32-bit integer.
    ///
    /// # Errors
    ///
    /// Returns [`FwError::InvalidValue`] unless the value is a `U32` or a
    /// four-byte firmware cell.
    pub fn as_u32(self) -> Result<u32, FwError> {
        match self.value {
            FwPropertyValue::U32(value) => Ok(value),
            FwPropertyValue::Bytes(bytes) if bytes.len() == 4 => {
                Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
            }
            _ => Err(FwError::InvalidValue),
        }
    }

    /// Interprets the value as a big-endian 64-bit integer.
    ///
    /// # Errors
    ///
    /// Returns [`FwError::InvalidValue`] unless the value is a `U64`, a `U32`,
    /// or an eight-byte firmware cell.
    pub fn as_u64(self) -> Result<u64, FwError> {
        match self.value {
            FwPropertyValue::U64(value) => Ok(value),
            FwPropertyValue::U32(value) => Ok(u64::from(value)),
            FwPropertyValue::Bytes(bytes) if bytes.len() == 8 => {
                let mut value = [0u8; 8];
                value.copy_from_slice(bytes);
                Ok(u64::from_be_bytes(value))
            }
            _ => Err(FwError::InvalidValue),
        }
    }

    /// Interprets the value as a string.
    ///
    /// Firmware byte strings are terminated by the first NUL; trailing bytes
    /// are ignored so a single entry of a string list can be read.
    ///
    /// # Errors
    ///
    /// Returns [`FwError::InvalidValue`] for a non-string value or invalid
    /// UTF-8.
    pub fn as_str(self) -> Result<&'a str, FwError> {
        match self.value {
            FwPropertyValue::Str(value) => Ok(value),
            FwPropertyValue::Bytes(bytes) => {
                let end = bytes
                    .iter()
                    .position(|byte| *byte == 0)
                    .unwrap_or(bytes.len());
                core::str::from_utf8(&bytes[..end]).map_err(|_| FwError::InvalidValue)
            }
            _ => Err(FwError::InvalidValue),
        }
    }

    /// Interprets the value as a boolean flag.
    ///
    /// A flag property is true; numeric properties are true when non-zero.
    ///
    /// # Errors
    ///
    /// Returns [`FwError::InvalidValue`] for a non-flag, non-numeric value.
    pub fn as_bool(self) -> Result<bool, FwError> {
        match self.value {
            FwPropertyValue::Bool(value) => Ok(value),
            FwPropertyValue::Empty => Ok(true),
            FwPropertyValue::U32(value) => Ok(value != 0),
            FwPropertyValue::Bytes([]) => Ok(true),
            _ => Err(FwError::InvalidValue),
        }
    }
}

/// One translated MMIO resource.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FwMmioResource<'a> {
    /// CPU physical start address.
    pub base: u64,
    /// Resource length in bytes.
    pub size: u64,
    /// Optional firmware resource name.
    pub name: Option<&'a str>,
}

/// One local DMA window declared between a bus and its parent.
///
/// The address fields are firmware facts: `bus_addr` is in the child bus
/// address space and `parent_addr` is in the immediate parent address space.
/// Neither field is necessarily a CPU physical address.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FwDmaRange {
    /// First address visible on the child bus.
    pub bus_addr: u64,
    /// First address in the parent bus.
    pub parent_addr: u64,
    /// Window length in bytes.
    pub size: u64,
}

/// The node's local DMA range declaration.
///
/// This is a declaration, not a device DMA mask or a permission to program
/// hardware. Ancestor translation and platform policy belong to the consumer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FwDmaRanges {
    /// No local declaration.
    Absent,
    /// An empty declaration; identity translation at this boundary only.
    Identity,
    /// This many explicit windows, indexed with [`FwNode::dma_range`].
    Windows(usize),
}

/// A device DMA window resolved and translated by the firmware backend.
///
/// `bus_addr` is the first device-visible address and `cpu_addr` the
/// corresponding first CPU physical address; `size` is the window length. The
/// window is not a device mask and does not by itself authorize DMA.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FwDmaWindow {
    /// First address in the device's bus address space.
    pub bus_addr: u64,
    /// First CPU physical address corresponding to `bus_addr`.
    pub cpu_addr: u64,
    /// Window length in bytes.
    pub size: u64,
}

/// Why firmware's DMA description cannot be used.
///
/// Absence of a declaration is not represented here; it is `Ok(None)`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FwDmaError {
    /// The backend failed to read or interpret its description.
    ///
    /// An absent declaration is not reported here: it is `Ok(None)`.
    Firmware(FwError),
    /// The DMA-parent path declares more than one window.
    MultiWindow,
    /// A firmware interconnect overrides the structural DMA parent.
    UnsupportedParent,
    /// An IOMMU or IOMMU map controls this device's addresses.
    Iommu,
    /// The declared window cannot be translated to the CPU address space.
    UnsupportedTranslation,
    /// The resolved window is empty, ambiguous or overflows an address space.
    InvalidWindow,
}

/// Fixed-capacity firmware parameter cells.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct FwParams {
    len: u8,
    data: [u32; FW_IRQ_SPEC_MAX_PARAMS],
}

impl fmt::Debug for FwParams {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_list().entries(self.as_slice()).finish()
    }
}

impl FwParams {
    /// Creates an empty parameter list.
    pub const fn empty() -> Self {
        Self {
            len: 0,
            data: [0; FW_IRQ_SPEC_MAX_PARAMS],
        }
    }

    /// Creates a parameter list from firmware cells.
    ///
    /// # Errors
    ///
    /// Returns [`FwError::BufferTooSmall`] when more than
    /// [`FW_IRQ_SPEC_MAX_PARAMS`] cells are supplied.
    pub fn new(values: &[u32]) -> Result<Self, FwError> {
        if values.len() > FW_IRQ_SPEC_MAX_PARAMS {
            return Err(FwError::BufferTooSmall);
        }
        let mut params = Self::empty();
        params.data[..values.len()].copy_from_slice(values);
        params.len = values.len() as u8;
        Ok(params)
    }

    /// Returns the number of parameter cells.
    pub fn len(&self) -> usize {
        usize::from(self.len)
    }

    /// Returns whether there are no parameter cells.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns the parameter cells.
    pub fn as_slice(&self) -> &[u32] {
        &self.data[..usize::from(self.len)]
    }

    /// Returns parameter cell `index`.
    pub fn get(&self, index: usize) -> Option<u32> {
        self.as_slice().get(index).copied()
    }
}

/// One interrupt specifier as described by firmware.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FwIrqSpec<'a> {
    /// Interrupt controller node that owns the specifier.
    pub controller: FwNode<'a>,
    /// Raw binding cells, in firmware order.
    pub params: FwParams,
}

/// One resolved firmware reference.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FwReference<'a> {
    /// Referenced node.
    pub node: FwNode<'a>,
    /// Argument cells that followed the reference, in firmware order.
    pub args: FwParams,
}

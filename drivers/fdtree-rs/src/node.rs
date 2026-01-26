// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2025 WeiKang Guo <guoweikang.kernel@gmail.com
// Copyright (C) 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSE for license details.

use crate::{
    parsing::{BigEndianU32, BigEndianU64, CStr, FdtData},
    standard_nodes::{Compatible, RegIter},
    LinuxFdt,
};

const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
pub(crate) const FDT_NOP: u32 = 4;
const FDT_END: u32 = 5;

#[derive(Debug, Clone, Copy)]
#[repr(C)]
struct FdtProperty {
    len: BigEndianU32,
    name_offset: BigEndianU32,
}

impl FdtProperty {
    fn from_bytes(bytes: &mut FdtData<'_>) -> Option<Self> {
        let len = bytes.u32()?;
        let name_offset = bytes.u32()?;

        Some(Self { len, name_offset })
    }
}

/// A devicetree node representing a device, bus, or other hardware component.
///
/// Nodes contain properties describing the hardware and may have child nodes
/// representing sub-components or devices.
#[derive(Debug, Clone, Copy)]
pub struct FdtNode<'b, 'a> {
    /// Node name (may include unit address, e.g., "uart@10000000")
    pub name: &'a str,
    pub(crate) header: &'b LinuxFdt<'a>,
    props: &'a [u8],
    parent_props: Option<&'a [u8]>,
}

impl<'b, 'a: 'b> FdtNode<'b, 'a> {
    fn new(
        name: &'a str,
        header: &'b LinuxFdt<'a>,
        props: &'a [u8],
        parent_props: Option<&'a [u8]>,
    ) -> Self {
        Self { name, header, props, parent_props }
    }

    /// Returns an iterator over the available properties of the node.
    pub fn properties(self) -> impl Iterator<Item = NodeProperty<'a>> + 'b {
        let mut stream = FdtData::new(self.props);
        let mut done = false;

        core::iter::from_fn(move || {
            if stream.is_empty() || done {
                return None;
            }

            while stream.peek_u32()?.get() == FDT_NOP {
                stream.skip(4);
            }

            if stream.peek_u32().unwrap().get() == FDT_PROP {
                Some(NodeProperty::parse(&mut stream, self.header))
            } else {
                done = true;
                None
            }
        })
    }

    /// Attempts to find a property by its name.
    ///
    /// Returns `None` if no property with the given name exists.
    pub fn property(self, name: &str) -> Option<NodeProperty<'a>> {
        self.properties().find(|p| p.name == name)
    }

    /// Check if the node is available (status property is "okay", "ok", or missing).
    ///
    /// Nodes with status="disabled" or other values are considered unavailable.
    pub fn is_available(self) -> bool {
        matches!(
            self.property("status")
                .and_then(|p| core::str::from_utf8(p.value).ok())
                .map(|s| s.trim_end_matches('\0')),
            None | Some("okay") | Some("ok")
        )
    }

    /// Returns an iterator over the child nodes of the current node.
    pub fn children(self) -> impl Iterator<Item = FdtNode<'b, 'a>> {
        let mut stream = FdtData::new(self.props);

        while stream.peek_u32().unwrap().get() == FDT_NOP {
            stream.skip(4);
        }

        while stream.peek_u32().unwrap().get() == FDT_PROP {
            NodeProperty::parse(&mut stream, self.header);
        }

        let mut done = false;

        core::iter::from_fn(move || {
            if stream.is_empty() || done {
                return None;
            }

            while stream.peek_u32()?.get() == FDT_NOP {
                stream.skip(4);
            }

            if stream.peek_u32()?.get() == FDT_BEGIN_NODE {
                let origin = stream.remaining();
                let ret = {
                    stream.skip(4);
                    let unit_name = CStr::new(stream.remaining()).expect("unit name").as_str()?;
                    let full_name_len = unit_name.len() + 1;
                    stream.skip(full_name_len);

                    if full_name_len % 4 != 0 {
                        stream.skip(4 - (full_name_len % 4));
                    }

                    Some(Self::new(unit_name, self.header, stream.remaining(), Some(self.props)))
                };

                stream = FdtData::new(origin);

                skip_current_node(&mut stream, self.header);

                ret
            } else {
                done = true;
                None
            }
        })
    }

    /// Parse and return the `reg` property as an iterator of memory regions.
    ///
    /// The `reg` property describes address ranges for devices and memory regions.
    ///
    /// Important: this method assumes that the value(s) inside the `reg`
    /// property represent CPU-addressable addresses that are able to fit within
    /// the platform's pointer size (e.g. `#address-cells` and `#size-cells` are
    /// less than or equal to 2 for a 64-bit platform). If this is not the case
    /// or you're unsure of whether this applies to the node, it is recommended
    /// to use the [`FdtNode::property`] method to extract the raw value slice
    /// or use the provided [`FdtNode::raw_reg`] helper method to give you an
    /// iterator over the address and size slices. One example of where this
    /// would return `None` for a node is a `pci` child node which contains the
    /// PCI address information in the `reg` property, of which the address has
    /// an `#address-cells` value of 3.
    ///
    /// Returns `None` if the node has no `reg` property or if cell sizes are too large.
    pub fn reg(self) -> Option<RegIter<'a>> {
        let sizes = self.parent_cell_sizes();
        if sizes.address_cells > 2 || sizes.size_cells > 2 {
            return None;
        }

        if let Some(reg) = self.property("reg") {
            return Some(RegIter::new(FdtData::new(reg.value), sizes));
        }

        None
    }

    /// Convenience method that provides an iterator over the raw bytes for the
    /// address and size values inside of the `reg` property.
    ///
    /// Use this method when working with nodes that have unusual cell sizes
    /// or when you need to manually parse the register values.
    pub fn raw_reg(self) -> Option<impl Iterator<Item = RawReg<'a>> + 'a> {
        let sizes = self.parent_cell_sizes();

        if let Some(prop) = self.property("reg") {
            let mut stream = FdtData::new(prop.value);
            return Some(core::iter::from_fn(move || {
                Some(RawReg {
                    address: stream.take(sizes.address_cells * 4)?,
                    size: stream.take(sizes.size_cells * 4)?,
                })
            }));
        }

        None
    }

    /// Parse and return the `compatible` property.
    ///
    /// The `compatible` property lists the device drivers that are compatible
    /// with this node, in order of preference.
    ///
    /// Returns `None` if the node has no `compatible` property.
    pub fn compatible(self) -> Option<Compatible<'a>> {
        let mut s = None;
        for prop in self.properties() {
            if prop.name == "compatible" {
                s = Some(Compatible { data: prop.value });
            }
        }

        s
    }

    /// Get the cell sizes (`#address-cells` and `#size-cells`) for child nodes.
    ///
    /// These values determine how addresses and sizes are encoded in child nodes.
    /// Returns default values (address_cells=2, size_cells=1) if not specified.
    pub fn cell_sizes(self) -> CellSizes {
        let mut cell_sizes = CellSizes::default();

        for property in self.properties() {
            match property.name {
                "#address-cells" => {
                    if let Some(val) = BigEndianU32::from_bytes(property.value) {
                        cell_sizes.address_cells = val.get() as usize;
                    }
                }
                "#size-cells" => {
                    if let Some(val) = BigEndianU32::from_bytes(property.value) {
                        cell_sizes.size_cells = val.get() as usize;
                    }
                }
                _ => {}
            }
        }

        cell_sizes
    }

    /// Search for and return the interrupt parent node.
    ///
    /// Returns the node referenced by the `interrupt-parent` property, if present.
    pub fn interrupt_parent(self) -> Option<FdtNode<'b, 'a>> {
        self.properties()
            .find(|p| p.name == "interrupt-parent")
            .and_then(|p| self.header.find_phandle(BigEndianU32::from_bytes(p.value)?.get()))
    }

    /// Get the value of the `#interrupt-cells` property.
    ///
    /// This determines how many cells are used to encode interrupt specifiers
    /// for child nodes. Returns `None` if the property is not present or invalid.
    pub fn interrupt_cells(self) -> Option<usize> {
        let mut interrupt_cells = None;

        if let Some(prop) = self.property("#interrupt-cells") {
            interrupt_cells = BigEndianU32::from_bytes(prop.value).map(|n| n.get() as usize)
        }

        interrupt_cells
    }

    /// Parse and return the `interrupts` property as an iterator.
    ///
    /// Returns an iterator over interrupt specifiers. The format depends on
    /// the interrupt controller's `#interrupt-cells` property.
    /// Returns `None` if parsing fails or the property is not present.
    pub fn interrupts(self) -> Option<impl Iterator<Item = usize> + 'a> {
        let sizes = self.parent_interrupt_cells()?;

        let mut interrupt = None;
        for prop in self.properties() {
            if prop.name == "interrupts" {
                let mut stream = FdtData::new(prop.value);
                interrupt = Some(core::iter::from_fn(move || {
                    let interrupt = match sizes {
                        1 => stream.u32()?.get() as usize,
                        2 => stream.u64()?.get() as usize,
                        _ => return None,
                    };

                    Some(interrupt)
                }));
                break;
            }
        }

        interrupt
    }

    pub(crate) fn parent_cell_sizes(self) -> CellSizes {
        let mut cell_sizes = CellSizes::default();

        if let Some(parent) = self.parent_props {
            let parent =
                FdtNode { name: "", props: parent, header: self.header, parent_props: None };
            cell_sizes = parent.cell_sizes();
        }

        cell_sizes
    }

    pub(crate) fn parent_interrupt_cells(self) -> Option<usize> {
        let mut interrupt_cells = None;
        let parent = self
            .property("interrupt-parent")
            .and_then(|p| self.header.find_phandle(BigEndianU32::from_bytes(p.value)?.get()))
            .or_else(|| {
                Some(FdtNode {
                    name: "",
                    props: self.parent_props?,
                    header: self.header,
                    parent_props: None,
                })
            });

        if let Some(size) = parent.and_then(|parent| parent.interrupt_cells()) {
            interrupt_cells = Some(size);
        }

        interrupt_cells
    }
}

/// The number of cells (big endian u32s) that addresses and sizes take.
///
/// In the device tree, addresses and sizes are encoded as sequences of 32-bit
/// big-endian values. The `#address-cells` and `#size-cells` properties specify
/// how many such values are used.
///
/// For example:
/// - `#address-cells = 2, #size-cells = 1` means addresses are 64-bit (2 cells)
///   and sizes are 32-bit (1 cell), typical for 64-bit systems
/// - `#address-cells = 1, #size-cells = 1` means both are 32-bit (1 cell each),
///   typical for 32-bit systems
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellSizes {
    /// Size of values representing an address (in u32 cells)
    pub address_cells: usize,
    /// Size of values representing a size (in u32 cells)
    pub size_cells: usize,
}

impl CellSizes {
    /// Validate that cell sizes are within reasonable bounds.
    ///
    /// Returns `false` if cell sizes are 0 or greater than 4, which would
    /// indicate an invalid or corrupt device tree.
    pub fn is_valid(&self) -> bool {
        self.address_cells > 0 && self.address_cells <= 4
            && self.size_cells > 0 && self.size_cells <= 4
    }
}

impl Default for CellSizes {
    /// Returns default cell sizes (address_cells=2, size_cells=1).
    ///
    /// These defaults represent 64-bit addresses and 32-bit sizes, which is
    /// common for modern 64-bit systems.
    fn default() -> Self {
        CellSizes { address_cells: 2, size_cells: 1 }
    }
}

/// A raw `reg` property value set containing unparsed address and size bytes.
///
/// Use this when working with nodes that have unusual cell sizes or when
/// manual parsing is required.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RawReg<'a> {
    /// Big-endian encoded bytes making up the address portion of the property.
    /// Length will always be a multiple of 4 bytes.
    pub address: &'a [u8],
    /// Big-endian encoded bytes making up the size portion of the property.
    /// Length will always be a multiple of 4 bytes.
    pub size: &'a [u8],
}

pub(crate) fn find_node<'b, 'a: 'b>(
    stream: &mut FdtData<'a>,
    name: &str,
    header: &'b LinuxFdt<'a>,
    parent_props: Option<&'a [u8]>,
) -> Option<FdtNode<'b, 'a>> {
    let mut parts = name.splitn(2, '/');
    let looking_for = parts.next()?;

    stream.skip_nops();

    let curr_data = stream.remaining();

    match stream.u32()?.get() {
        FDT_BEGIN_NODE => {}
        _ => return None,
    }

    let unit_name = CStr::new(stream.remaining()).expect("unit name C str").as_str()?;

    let full_name_len = unit_name.len() + 1;
    skip_4_aligned(stream, full_name_len);

    let looking_contains_addr = looking_for.contains('@');
    let addr_name_same = unit_name == looking_for;
    let base_name_same = unit_name.split('@').next()? == looking_for;

    if (looking_contains_addr && !addr_name_same) || (!looking_contains_addr && !base_name_same) {
        *stream = FdtData::new(curr_data);
        skip_current_node(stream, header);

        return None;
    }

    let next_part = match parts.next() {
        None | Some("") => {
            return Some(FdtNode::new(unit_name, header, stream.remaining(), parent_props))
        }
        Some(part) => part,
    };

    stream.skip_nops();

    let parent_props = Some(stream.remaining());

    while stream.peek_u32()?.get() == FDT_PROP {
        let _ = NodeProperty::parse(stream, header);
    }

    while stream.peek_u32()?.get() == FDT_BEGIN_NODE {
        if let Some(p) = find_node(stream, next_part, header, parent_props) {
            return Some(p);
        }
    }

    stream.skip_nops();

    if stream.u32()?.get() != FDT_END_NODE {
        return None;
    }

    None
}

// FIXME: this probably needs refactored
pub(crate) fn all_nodes<'b, 'a: 'b>(header: &'b LinuxFdt<'a>) -> impl Iterator<Item = FdtNode<'b, 'a>> {
    let mut stream = FdtData::new(header.structs_block());
    let mut done = false;
    let mut parents: [&[u8]; 64] = [&[]; 64];
    let mut parent_index = 0;

    core::iter::from_fn(move || {
        if stream.is_empty() || done {
            return None;
        }

        while stream.peek_u32()?.get() == FDT_END_NODE {
            parent_index -= 1;
            stream.skip(4);
        }

        if stream.peek_u32()?.get() == FDT_END {
            done = true;
            return None;
        }

        while stream.peek_u32()?.get() == FDT_NOP {
            stream.skip(4);
        }

        match stream.u32()?.get() {
            FDT_BEGIN_NODE => {}
            _ => return None,
        }

        let unit_name = CStr::new(stream.remaining()).expect("unit name C str").as_str().unwrap();
        let full_name_len = unit_name.len() + 1;
        skip_4_aligned(&mut stream, full_name_len);

        let curr_node = stream.remaining();

        parent_index += 1;
        parents[parent_index] = curr_node;

        while stream.peek_u32()?.get() == FDT_NOP {
            stream.skip(4);
        }

        while stream.peek_u32()?.get() == FDT_PROP {
            NodeProperty::parse(&mut stream, header);
        }

        Some(FdtNode {
            name: if unit_name.is_empty() { "/" } else { unit_name },
            header,
            parent_props: match parent_index {
                1 => None,
                _ => Some(parents[parent_index - 1]),
            },
            props: curr_node,
        })
    })
}

pub(crate) fn skip_current_node<'a>(stream: &mut FdtData<'a>, header: &LinuxFdt<'a>) {
    assert_eq!(stream.u32().unwrap().get(), FDT_BEGIN_NODE, "bad node");

    let unit_name = CStr::new(stream.remaining()).expect("unit_name C str").as_str().unwrap();
    let full_name_len = unit_name.len() + 1;
    skip_4_aligned(stream, full_name_len);

    while stream.peek_u32().unwrap().get() == FDT_PROP {
        NodeProperty::parse(stream, header);
    }

    while stream.peek_u32().unwrap().get() == FDT_BEGIN_NODE {
        skip_current_node(stream, header);
    }

    stream.skip_nops();

    assert_eq!(stream.u32().unwrap().get(), FDT_END_NODE, "bad node");
}

/// A devicetree node property containing a name and value.
///
/// Properties describe attributes of nodes such as device configuration,
/// addresses, interrupts, etc.
#[derive(Debug, Clone, Copy)]
pub struct NodeProperty<'a> {
    /// Property name
    pub name: &'a str,
    /// Property value (raw bytes)
    pub value: &'a [u8],
}

impl<'a> NodeProperty<'a> {
    /// Attempt to parse the property value as a `usize`.
    ///
    /// Handles both 32-bit (4 bytes) and 64-bit (8 bytes) values.
    /// Returns `None` if the value length is not 4 or 8 bytes, or if parsing fails.
    pub fn as_usize(self) -> Option<usize> {
        match self.value.len() {
            4 => BigEndianU32::from_bytes(self.value).map(|i| i.get() as usize),
            8 => BigEndianU64::from_bytes(self.value).map(|i| i.get() as usize),
            _ => None,
        }
    }

    /// Attempt to parse the property value as a UTF-8 string.
    ///
    /// Automatically trims null terminators from the end of the string.
    /// Returns `None` if the value is not valid UTF-8.
    pub fn as_str(self) -> Option<&'a str> {
        core::str::from_utf8(self.value).map(|s| s.trim_end_matches('\0')).ok()
    }

    fn parse(stream: &mut FdtData<'a>, header: &LinuxFdt<'a>) -> Self {
        match stream.u32().unwrap().get() {
            FDT_PROP => {}
            other => panic!("bad prop, tag: {}", other),
        }

        let prop = FdtProperty::from_bytes(stream).expect("FDT property");
        let data_len = prop.len.get() as usize;

        let data = &stream.remaining()[..data_len];

        skip_4_aligned(stream, data_len);

        NodeProperty { name: header.str_at_offset(prop.name_offset.get() as usize), value: data }
    }

    /// Attempt to parse the property value as a `reg` property.
    ///
    /// The `reg` property describes address ranges for devices and memory regions.
    /// Returns `None` if cell sizes are too large (>2) or parsing fails.
    ///
    /// # Arguments
    /// * `sizes` - The cell sizes to use for parsing addresses and sizes
    pub fn as_reg(self, sizes:  CellSizes) -> Option<RegIter<'a>> {
        if sizes.address_cells > 2 || sizes.size_cells > 2 {
            return None;
        }
        Some(RegIter::new(FdtData::new(self.value), sizes))
    }
}

/// Standard memory reservation from the FDT header's memory reservation block.
///
/// These reservations describe physical memory regions that should not be used
/// by the operating system. They are stored in a separate block referenced by
/// the FDT header's `off_mem_rsvmap` field.
///
/// A 32-bit (big-endian) offset field in the FDT header,
/// relative to the start address of the DTB.
/// Points to an array of fdt_reserve_entry entries, each element of which is fixed:
/// - address: be64 (physical start address)
/// - size: be64 (length)
///
/// Terminated by a pair of zeros: address == 0 && size == 0.
/// Not dependent on #address-cells / #size-cells, always 64-bit big-endian encoding.
///
/// Semantics: These ranges are reserved for firmware, secure world, device buffers,
/// etc. The OS should remove them from available memory early during memory
/// initialization (e.g., memblock_reserve in Linux).
#[derive(Debug)]
#[repr(C)]
pub struct MemoryReservation {
    pub(crate) address: BigEndianU64,
    pub(crate) size: BigEndianU64,
}

impl MemoryReservation {
    /// Returns a pointer representing the memory reservation address.
    pub fn address(&self) -> *const u8 {
        self.address.get() as usize as *const u8
    }

    /// Returns the size of the memory reservation in bytes.
    pub fn size(&self) -> usize {
        self.size.get() as usize
    }

    pub(crate) fn from_bytes(bytes: &mut FdtData<'_>) -> Option<Self> {
        let address = bytes.u64()?;
        let size = bytes.u64()?;

        Some(Self { address, size })
    }
}

fn skip_4_aligned(stream: &mut FdtData<'_>, len: usize) {
    stream.skip((len + 3) & !0x3);
}

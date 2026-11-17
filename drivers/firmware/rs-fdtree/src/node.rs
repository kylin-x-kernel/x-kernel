// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use crate::{
    LinuxFdt, PropertyError,
    parsing::{BigEndianU32, CStr, FdtData},
};

const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
pub(crate) const FDT_NOP: u32 = 4;
const FDT_END: u32 = 9;
const MAX_NODE_DEPTH: usize = 63;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct FdtProperty {
    len: crate::parsing::BigEndianU32,
    name_offset: crate::parsing::BigEndianU32,
}

impl FdtProperty {
    fn from_bytes(bytes: &mut FdtData<'_>) -> Option<Self> {
        Some(Self {
            len: bytes.u32()?,
            name_offset: bytes.u32()?,
        })
    }
}

/// A device-tree node: its name plus borrowed views of its property bytes
/// and (optionally) its parent's property bytes for `#*-cells` lookups.
#[derive(Clone, Copy, Debug)]
pub struct FdtNode<'b, 'a> {
    /// Node name (may include unit address, e.g., "uart@10000000")
    pub name: &'a str,
    pub(crate) header: &'b LinuxFdt<'a>,
    offset: u32,
    props: &'a [u8],
    parent_props: Option<&'a [u8]>,
    /// Effective `interrupt-parent` for this node, inherited during the
    /// traversal that produced it.
    ///
    /// `Ok(Some(phandle))` is the nearest declaration, `Ok(None)` means no
    /// declaration in the ancestor chain, and `Err` records a malformed or
    /// duplicated declaration that must not be mistaken for absence.
    interrupt_parent: Result<Option<u32>, PropertyError>,
}

/// The borrowed pieces of a node, kept by allocation-backed indexes so a
/// node can be rebuilt without scanning the structure block again.
#[cfg(feature = "alloc")]
#[derive(Clone, Copy, Debug)]
pub(crate) struct NodeParts<'a> {
    pub(crate) name: &'a str,
    pub(crate) offset: u32,
    pub(crate) props: &'a [u8],
    pub(crate) parent_props: Option<&'a [u8]>,
    pub(crate) interrupt_parent: Result<Option<u32>, PropertyError>,
}

impl<'b, 'a: 'b> FdtNode<'b, 'a> {
    /// Returns the effective `interrupt-parent` phandle computed while
    /// traversing the tree, including declarations inherited from ancestors.
    pub(crate) fn effective_interrupt_parent(self) -> Result<Option<u32>, PropertyError> {
        self.interrupt_parent
    }

    /// Returns the borrowed pieces this node was built from.
    #[cfg(feature = "alloc")]
    pub(crate) fn parts(self) -> NodeParts<'a> {
        NodeParts {
            name: self.name,
            offset: self.offset,
            props: self.props,
            parent_props: self.parent_props,
            interrupt_parent: self.interrupt_parent,
        }
    }

    /// Rebuilds a node from pieces captured during an earlier traversal.
    #[cfg(feature = "alloc")]
    pub(crate) fn from_parts(header: &'b LinuxFdt<'a>, parts: NodeParts<'a>) -> Self {
        Self {
            name: parts.name,
            header,
            offset: parts.offset,
            props: parts.props,
            parent_props: parts.parent_props,
            interrupt_parent: parts.interrupt_parent,
        }
    }

    /// Byte offset of this node's BEGIN_NODE token within the structure block.
    ///
    /// This is local to the tree, not a phandle or a globally unique identity.
    /// Use LinuxFdt::node_at_offset to validate a stored offset on lookup.
    pub fn offset(self) -> u32 {
        self.offset
    }

    /// Returns the borrowed parser view, not a canonical firmware identity.
    pub fn as_tree(self) -> &'b LinuxFdt<'a> {
        self.header
    }

    /// Return the structural parent, or `None` for the root.
    ///
    /// Uses property-slice identity, not potentially duplicated node names.
    /// This allocation-free lookup scans the tree: O(number of nodes), intended
    /// for firmware discovery rather than runtime device operations.
    pub fn parent(self) -> Option<Self> {
        let props = self.parent_props?;
        self.header
            .all_nodes()
            .find(|node| node.props.as_ptr() == props.as_ptr() && node.props.len() == props.len())
    }

    /// Returns the first direct child, or `None` for a leaf.
    ///
    /// Uses property-slice identity, not names. This allocation-free lookup
    /// scans the tree: O(number of nodes), intended for firmware discovery.
    pub fn first_child(self) -> Option<Self> {
        let props = self.props;
        self.header.all_nodes().find(|node| {
            node.parent_props
                .is_some_and(|parent| parent.as_ptr() == props.as_ptr())
        })
    }

    /// Returns the next direct sibling, or `None` for the last child.
    ///
    /// This allocation-free lookup scans the tree: O(number of nodes),
    /// intended for firmware discovery.
    pub fn next_sibling(self) -> Option<Self> {
        let parent_props = self.parent_props.map(|props| props.as_ptr());
        let mut after_self = false;
        for node in self.header.all_nodes() {
            if node.offset == self.offset {
                after_self = true;
                continue;
            }
            if after_self && node.parent_props.map(|props| props.as_ptr()) == parent_props {
                return Some(node);
            }
        }
        None
    }

    /// Returns an iterator over the direct children.
    pub fn children(self) -> impl Iterator<Item = Self> + 'b {
        let mut next = self.first_child();
        core::iter::from_fn(move || {
            let node = next?;
            next = node.next_sibling();
            Some(node)
        })
    }

    /// Returns an iterator over the current node properties.
    pub fn properties(self) -> impl Iterator<Item = NodeProperty<'a>> + 'b {
        properties(self.header, self.props)
    }

    /// Returns the named property if present.
    pub fn property(self, name: &str) -> Option<NodeProperty<'a>> {
        self.properties().find(|prop| prop.name == name)
    }

    /// Returns the named property interpreted as a C-string.
    pub fn property_str(self, name: &str) -> Option<&'a str> {
        let prop = self.property(name)?;
        CStr::new(prop.value)?.as_str()
    }

    /// Returns the named property interpreted as a big-endian u32.
    pub fn property_u32(self, name: &str) -> Option<u32> {
        Some(BigEndianU32::from_bytes(self.property(name)?.value)?.get())
    }

    /// Returns the named property from the direct parent node if present.
    pub fn parent_property(self, name: &str) -> Option<NodeProperty<'a>> {
        let props = self.parent_props?;
        let mut stream = FdtData::new(props);
        let mut done = false;

        core::iter::from_fn(move || {
            if stream.is_empty() || done {
                return None;
            }

            stream.skip_nops();
            if stream.peek_u32()?.get() != FDT_PROP {
                done = true;
                return None;
            }

            Some(NodeProperty::parse(&mut stream, self.header))
        })
        .find(|prop| prop.name == name)
    }

    /// Returns the named property from the direct parent node interpreted as a
    /// big-endian u32.
    pub fn parent_property_u32(self, name: &str) -> Option<u32> {
        Some(BigEndianU32::from_bytes(self.parent_property(name)?.value)?.get())
    }

    /// Returns an iterator over all compatible strings, in firmware order.
    pub fn compatibles(self) -> impl Iterator<Item = &'a str> + 'b {
        let mut bytes = self
            .property("compatible")
            .map(|prop| prop.value)
            .unwrap_or(&[]);
        core::iter::from_fn(move || {
            if bytes.is_empty() {
                return None;
            }
            let end = bytes.iter().position(|&b| b == 0)?;
            let compat = core::str::from_utf8(&bytes[..end]).ok()?;
            bytes = &bytes[end + 1..];
            Some(compat)
        })
    }

    /// Returns the first compatible string if present.
    pub fn compatible(self) -> Option<&'a str> {
        self.compatibles().next()
    }

    /// Returns whether the node matches any compatible string in its list.
    pub fn is_compatible(self, compatible: &str) -> bool {
        self.compatibles().any(|candidate| candidate == compatible)
    }
}

fn interrupt_parent_cell(value: &[u8]) -> Result<u32, PropertyError> {
    let value: [u8; 4] = value
        .try_into()
        .map_err(|_| PropertyError::InvalidEncoding)?;
    Ok(u32::from_be_bytes(value))
}

fn merge_interrupt_parent(
    declared: Option<Result<u32, PropertyError>>,
    inherited: Result<Option<u32>, PropertyError>,
) -> Result<Option<u32>, PropertyError> {
    match declared {
        Some(Ok(phandle)) => Ok(Some(phandle)),
        Some(Err(error)) => Err(error),
        None => inherited,
    }
}

/// Returns the effective `interrupt-parent` declared in one node's property
/// stream, falling back to the value inherited from its ancestors.
///
/// Scans only this node's properties; traversal callers normally capture the
/// declaration while already walking the same property stream.
fn scan_interrupt_parent<'a>(
    props: &'a [u8],
    header: &LinuxFdt<'a>,
    inherited: Result<Option<u32>, PropertyError>,
) -> Result<Option<u32>, PropertyError> {
    let mut stream = FdtData::new(props);
    let mut declared = None;
    loop {
        stream.skip_nops();
        let Some(tag) = stream.peek_u32() else {
            break;
        };
        if tag.get() != FDT_PROP {
            break;
        }
        let property = NodeProperty::parse(&mut stream, header);
        if property.name == "interrupt-parent" {
            let value = interrupt_parent_cell(property.value);
            declared = Some(if declared.is_some() {
                Err(PropertyError::Duplicate)
            } else {
                value
            });
        }
        stream.skip_nops();
    }
    merge_interrupt_parent(declared, inherited)
}

pub(crate) fn all_nodes<'b, 'a: 'b>(
    header: &'b LinuxFdt<'a>,
) -> impl Iterator<Item = FdtNode<'b, 'a>> {
    let mut stream = FdtData::new(header.structs_block());
    let mut done = false;
    let mut parents: [&[u8]; MAX_NODE_DEPTH + 1] = [&[]; MAX_NODE_DEPTH + 1];
    let mut interrupt_parents: [Result<Option<u32>, PropertyError>; MAX_NODE_DEPTH + 1] =
        [Ok(None); MAX_NODE_DEPTH + 1];
    let mut parent_index = 0usize;

    core::iter::from_fn(move || {
        if stream.is_empty() || done {
            return None;
        }

        stream.skip_nops();
        while stream.peek_u32()?.get() == FDT_END_NODE {
            parent_index = parent_index.saturating_sub(1);
            stream.skip(4);
            stream.skip_nops();
        }

        if stream.peek_u32()?.get() == FDT_END {
            done = true;
            return None;
        }

        stream.skip_nops();
        let offset = (header.structs_block().len() - stream.remaining().len()) as u32;
        if stream.u32()?.get() != FDT_BEGIN_NODE {
            return None;
        }

        let unit_name = CStr::new(stream.remaining())?.as_str()?;
        let full_name_len = unit_name.len() + 1;
        skip_4_aligned(&mut stream, full_name_len);
        let curr_node = stream.remaining();

        parent_index += 1;
        parents[parent_index] = curr_node;
        let inherited = interrupt_parents[parent_index - 1];

        let mut declared = None;
        stream.skip_nops();
        while stream.peek_u32()?.get() == FDT_PROP {
            let property = NodeProperty::parse(&mut stream, header);
            if property.name == "interrupt-parent" {
                let value = interrupt_parent_cell(property.value);
                declared = Some(if declared.is_some() {
                    Err(PropertyError::Duplicate)
                } else {
                    value
                });
            }
            stream.skip_nops();
        }
        let interrupt_parent = merge_interrupt_parent(declared, inherited);
        interrupt_parents[parent_index] = interrupt_parent;

        Some(FdtNode {
            name: if unit_name.is_empty() { "/" } else { unit_name },
            header,
            offset,
            props: curr_node,
            parent_props: if parent_index == 1 {
                None
            } else {
                Some(parents[parent_index - 1])
            },
            interrupt_parent,
        })
    })
}

/// A raw property: its name and uninterpreted big-endian payload bytes.
#[derive(Clone, Copy, Debug)]
pub struct NodeProperty<'a> {
    /// Property name as stored in the strings block.
    pub name: &'a str,
    /// Property payload bytes, in firmware byte order.
    pub value: &'a [u8],
}

impl<'a> NodeProperty<'a> {
    fn parse(stream: &mut FdtData<'a>, header: &LinuxFdt<'a>) -> Self {
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: LinuxFdt construction validates every \
                      property header, string offset and node name before traversal"
        )]
        let tag = stream.u32().expect("FDT property tag").get();
        assert_eq!(tag, FDT_PROP, "bad prop, tag: {}", tag);

        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: LinuxFdt construction validates every \
                      property header, string offset and node name before traversal"
        )]
        let prop = FdtProperty::from_bytes(stream).expect("FDT property");
        let data_len = prop.len.get() as usize;
        let data = &stream.remaining()[..data_len];
        skip_4_aligned(stream, data_len);

        NodeProperty {
            name: {
                #[expect(
                    clippy::expect_used,
                    reason = "CG-021-no-panic-on-reachable-failure: LinuxFdt construction \
                              validates every property header, string offset and node name before \
                              traversal"
                )]
                header
                    .string_at_offset(prop.name_offset.get() as usize)
                    .expect("invalid FDT string offset")
            },
            value: data,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CellSizes {
    pub address_cells: usize,
    pub size_cells: usize,
}

impl Default for CellSizes {
    fn default() -> Self {
        Self {
            address_cells: 2,
            size_cells: 1,
        }
    }
}

/// A physical memory region described by the device tree: a start address
/// and a size in bytes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MemoryRegion {
    /// Start address (physical for firmware-described regions).
    pub starting_address: *const u8,
    /// Region size in bytes.
    pub size: usize,
}

/// Iterator over the `reg` entries of one node, decoding address/size
/// pairs with the node's `#address-cells` / `#size-cells` widths.
#[derive(Clone, Debug)]
pub struct RegIter<'a> {
    stream: FdtData<'a>,
    sizes: CellSizes,
}

impl<'a> RegIter<'a> {
    /// Creates an iterator over `reg` entries decoded with `sizes`.
    pub fn new(stream: FdtData<'a>, sizes: CellSizes) -> Self {
        Self { stream, sizes }
    }
}

impl<'a> Iterator for RegIter<'a> {
    type Item = MemoryRegion;

    fn next(&mut self) -> Option<Self::Item> {
        let base = match self.sizes.address_cells {
            1 => self.stream.u32()?.get() as usize,
            2 => self.stream.u64()?.get() as usize,
            _ => return None,
        } as *const u8;

        let size = match self.sizes.size_cells {
            1 => self.stream.u32()?.get() as usize,
            2 => self.stream.u64()?.get() as usize,
            _ => return None,
        };

        Some(MemoryRegion {
            starting_address: base,
            size,
        })
    }
}

impl<'b, 'a: 'b> FdtNode<'b, 'a> {
    /// Returns an iterator over this node's `reg` entries, honoring the
    /// parent's cell widths; `None` when the node has no `reg` property or
    /// the cell widths exceed the supported 2-cell maximum.
    pub fn reg(self) -> Option<RegIter<'a>> {
        let sizes = self.parent_cell_sizes();
        if sizes.address_cells > 2 || sizes.size_cells > 2 {
            return None;
        }

        Some(RegIter::new(
            FdtData::new(self.property("reg")?.value),
            sizes,
        ))
    }

    /// Returns this node's `#address-cells` / `#size-cells`, falling back
    /// to the spec defaults (2 and 1).
    pub fn cell_sizes(self) -> CellSizes {
        cell_sizes(self.properties())
    }

    fn parent_cell_sizes(self) -> CellSizes {
        self.parent_props.map_or_else(CellSizes::default, |parent| {
            cell_sizes(properties(self.header, parent))
        })
    }
}

fn cell_sizes<'a>(properties: impl Iterator<Item = NodeProperty<'a>>) -> CellSizes {
    let mut cell_sizes = CellSizes::default();

    for property in properties {
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

fn properties<'b, 'a: 'b>(
    header: &'b LinuxFdt<'a>,
    props: &'a [u8],
) -> impl Iterator<Item = NodeProperty<'a>> + 'b {
    let mut stream = FdtData::new(props);
    let mut done = false;

    core::iter::from_fn(move || {
        if stream.is_empty() || done {
            return None;
        }

        stream.skip_nops();
        if stream.peek_u32()?.get() != FDT_PROP {
            done = true;
            return None;
        }

        Some(NodeProperty::parse(&mut stream, header))
    })
}

pub(crate) fn find_node<'b, 'a: 'b>(
    stream: &mut FdtData<'a>,
    name: &str,
    header: &'b LinuxFdt<'a>,
    parent_props: Option<&'a [u8]>,
    inherited_interrupt_parent: Result<Option<u32>, PropertyError>,
) -> Option<FdtNode<'b, 'a>> {
    let mut parts = name.splitn(2, '/');
    let looking_for = parts.next()?;

    stream.skip_nops();
    let curr_data = stream.remaining();

    if stream.u32()?.get() != FDT_BEGIN_NODE {
        return None;
    }

    let unit_name = CStr::new(stream.remaining())?.as_str()?;
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
            return Some(FdtNode {
                name: unit_name,
                header,
                offset: (header.structs_block().len() - curr_data.len()) as u32,
                props: stream.remaining(),
                parent_props,
                interrupt_parent: scan_interrupt_parent(
                    stream.remaining(),
                    header,
                    inherited_interrupt_parent,
                ),
            });
        }
        Some(part) => part,
    };

    let parent_props = Some(stream.remaining());
    let mut declared = None;
    stream.skip_nops();

    while stream.peek_u32()?.get() == FDT_PROP {
        let property = NodeProperty::parse(stream, header);
        if property.name == "interrupt-parent" {
            let value = interrupt_parent_cell(property.value);
            declared = Some(if declared.is_some() {
                Err(PropertyError::Duplicate)
            } else {
                value
            });
        }
        stream.skip_nops();
    }
    let inherited_interrupt_parent = merge_interrupt_parent(declared, inherited_interrupt_parent);

    loop {
        stream.skip_nops();
        if stream.peek_u32()?.get() != FDT_BEGIN_NODE {
            break;
        }
        if let Some(node) = find_node(
            stream,
            next_part,
            header,
            parent_props,
            inherited_interrupt_parent,
        ) {
            return Some(node);
        }
    }

    stream.skip_nops();
    if stream.u32()?.get() != FDT_END_NODE {
        return None;
    }

    None
}

pub(crate) fn reserved_memory_regions<'b, 'a: 'b>(
    header: &'b LinuxFdt<'a>,
) -> impl Iterator<Item = MemoryRegion> + 'b {
    reserved_memory_nodes(header).flat_map(|node| node.reg().into_iter().flatten())
}

pub(crate) fn reserved_memory_nodes<'b, 'a: 'b>(
    header: &'b LinuxFdt<'a>,
) -> impl Iterator<Item = FdtNode<'b, 'a>> + 'b {
    let reserved = find_node(
        &mut FdtData::new(header.structs_block()),
        "/reserved-memory",
        header,
        None,
        Ok(None),
    )
    .map(|node| node.props);

    all_nodes(header).filter(move |node| {
        let Some(reserved_props) = reserved else {
            return false;
        };
        let Some(parent_props) = node.parent_props else {
            return false;
        };
        parent_props.as_ptr() == reserved_props.as_ptr()
            && parent_props.len() == reserved_props.len()
    })
}

pub(crate) fn memory_regions<'b, 'a: 'b>(
    header: &'b LinuxFdt<'a>,
) -> impl Iterator<Item = MemoryRegion> + 'b {
    all_nodes(header)
        .filter(|node| {
            let is_memory_name = node.name == "memory" || node.name.starts_with("memory@");
            let is_memory_type = node.property_str("device_type") == Some("memory");
            is_memory_name || is_memory_type
        })
        .flat_map(|node| node.reg().into_iter().flatten())
}

pub(crate) fn skip_current_node<'a>(stream: &mut FdtData<'a>, header: &LinuxFdt<'a>) {
    #[expect(
        clippy::unwrap_used,
        reason = "CG-021-no-panic-on-reachable-failure: constructor validation proves this node \
                  has a complete BEGIN_NODE token"
    )]
    let tag = stream.u32().unwrap().get();
    assert_eq!(tag, FDT_BEGIN_NODE, "bad node");
    #[expect(
        clippy::unwrap_used,
        reason = "CG-021-no-panic-on-reachable-failure: constructor validation proves every node \
                  name is NUL-terminated UTF-8"
    )]
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: LinuxFdt construction validates every \
                  property header, string offset and node name before traversal"
    )]
    let unit_name = CStr::new(stream.remaining())
        .expect("unit_name C str")
        .as_str()
        .unwrap();
    skip_4_aligned(stream, unit_name.len() + 1);

    stream.skip_nops();
    #[expect(
        clippy::unwrap_used,
        reason = "CG-021-no-panic-on-reachable-failure: validated property records and balanced \
                  nodes always leave a following token"
    )]
    while stream.peek_u32().unwrap().get() == FDT_PROP {
        NodeProperty::parse(stream, header);
        stream.skip_nops();
    }
    loop {
        stream.skip_nops();
        #[expect(
            clippy::unwrap_used,
            reason = "CG-021-no-panic-on-reachable-failure: constructor validation proves a token \
                      follows each property or completed child"
        )]
        let next = stream.peek_u32().unwrap().get();
        if next != FDT_BEGIN_NODE {
            break;
        }
        skip_current_node(stream, header);
    }
    stream.skip_nops();
    #[expect(
        clippy::unwrap_used,
        reason = "CG-021-no-panic-on-reachable-failure: constructor validation balances every \
                  BEGIN_NODE with a complete END_NODE token"
    )]
    let tag = stream.u32().unwrap().get();
    assert_eq!(tag, FDT_END_NODE, "bad node");
}

fn skip_4_aligned(stream: &mut FdtData<'_>, len: usize) {
    stream.skip((len + 3) & !0x3);
}

/// Validates the complete token grammar before a LinuxFdt can be published.
/// The bounded stack also proves the depth bound used by all_nodes/recursion.
pub(crate) fn validate_structure(bytes: &[u8], strings: &[u8]) -> Result<(), crate::FdtError> {
    use crate::FdtError::ParseError;

    let mut stream = FdtData::new(bytes);
    let mut has_children = [false; MAX_NODE_DEPTH];
    let mut depth = 0;
    loop {
        // This parser also runs at physical addresses before paging is enabled.
        // Keep NOPs and the final END outside the node-token dispatch: a dense
        // five-token match generates absolute virtual-address jump tables on
        // RISC-V, which cannot be used in that boot phase.
        stream.skip_nops();
        match stream.u32().ok_or(ParseError)?.get() {
            FDT_BEGIN_NODE => {
                if depth == MAX_NODE_DEPTH {
                    return Err(ParseError);
                }
                let name = CStr::new(stream.remaining())
                    .and_then(|name| name.as_str())
                    .ok_or(ParseError)?;
                if (depth == 0 && !name.is_empty())
                    || (depth != 0 && name.is_empty())
                    || name.contains('/')
                {
                    return Err(ParseError);
                }
                take_aligned(&mut stream, name.len().checked_add(1).ok_or(ParseError)?)?;
                if depth > 0 {
                    has_children[depth - 1] = true;
                }
                has_children[depth] = false;
                depth += 1;
            }
            FDT_END_NODE => {
                depth = depth.checked_sub(1).ok_or(ParseError)?;
                if depth == 0 {
                    stream.skip_nops();
                    return if stream.u32().ok_or(ParseError)?.get() == FDT_END && stream.is_empty()
                    {
                        Ok(())
                    } else {
                        Err(ParseError)
                    };
                }
            }
            FDT_PROP => {
                if depth == 0 || has_children[depth - 1] {
                    return Err(ParseError);
                }
                let property = FdtProperty::from_bytes(&mut stream).ok_or(ParseError)?;
                let name_bytes = strings
                    .get(property.name_offset.get() as usize..)
                    .ok_or(ParseError)?;
                let name = CStr::new(name_bytes)
                    .and_then(|name| name.as_str())
                    .ok_or(ParseError)?;
                if name.is_empty() {
                    return Err(ParseError);
                }
                take_aligned(&mut stream, property.len.get() as usize)?;
            }
            _ => return Err(ParseError),
        }
    }
}

fn take_aligned(stream: &mut FdtData<'_>, len: usize) -> Result<(), crate::FdtError> {
    use crate::FdtError::ParseError;
    let padded_len = len.checked_add(3).ok_or(ParseError)? & !3;
    // Padding is not interpreted. Some deployed DTBs (including crosvm.dtb)
    // retain nonzero bytes there; only its presence is needed for safe traversal.
    stream.take(padded_len).ok_or(ParseError)?;
    Ok(())
}

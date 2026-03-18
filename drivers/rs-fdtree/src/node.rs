// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use crate::{
    LinuxFdt,
    parsing::{BigEndianU32, CStr, FdtData},
};

const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
pub(crate) const FDT_NOP: u32 = 4;
const FDT_END: u32 = 5;

#[derive(Debug, Clone, Copy)]
#[repr(C)]
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

#[derive(Debug, Clone, Copy)]
pub struct FdtNode<'b, 'a> {
    /// Node name (may include unit address, e.g., "uart@10000000")
    pub name: &'a str,
    pub(crate) header: &'b LinuxFdt<'a>,
    props: &'a [u8],
    parent_props: Option<&'a [u8]>,
}

impl<'b, 'a: 'b> FdtNode<'b, 'a> {
    /// Returns an iterator over the current node properties.
    pub fn properties(self) -> impl Iterator<Item = NodeProperty<'a>> + 'b {
        let mut stream = FdtData::new(self.props);
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
    }

    /// Returns the named property if present.
    pub fn property(self, name: &str) -> Option<NodeProperty<'a>> {
        self.properties().find(|prop| prop.name == name)
    }

    /// Returns the first compatible string if present.
    pub fn compatible(self) -> Option<&'a str> {
        let property = self.property("compatible")?;
        CStr::new(property.value)?.as_str()
    }
}

pub(crate) fn all_nodes<'b, 'a: 'b>(
    header: &'b LinuxFdt<'a>,
) -> impl Iterator<Item = FdtNode<'b, 'a>> {
    let mut stream = FdtData::new(header.structs_block());
    let mut done = false;
    let mut parents: [&[u8]; 64] = [&[]; 64];
    let mut parent_index = 0usize;

    core::iter::from_fn(move || {
        if stream.is_empty() || done {
            return None;
        }

        while stream.peek_u32()?.get() == FDT_END_NODE {
            parent_index = parent_index.saturating_sub(1);
            stream.skip(4);
        }

        if stream.peek_u32()?.get() == FDT_END {
            done = true;
            return None;
        }

        stream.skip_nops();
        if stream.u32()?.get() != FDT_BEGIN_NODE {
            return None;
        }

        let unit_name = CStr::new(stream.remaining())?.as_str()?;
        let full_name_len = unit_name.len() + 1;
        skip_4_aligned(&mut stream, full_name_len);
        let curr_node = stream.remaining();

        parent_index += 1;
        parents[parent_index] = curr_node;

        while stream.peek_u32()?.get() == FDT_PROP {
            let _ = NodeProperty::parse(&mut stream, header);
        }

        Some(FdtNode {
            name: if unit_name.is_empty() { "/" } else { unit_name },
            header,
            props: curr_node,
            parent_props: if parent_index == 1 {
                None
            } else {
                Some(parents[parent_index - 1])
            },
        })
    })
}

#[derive(Debug, Clone, Copy)]
pub struct NodeProperty<'a> {
    pub name: &'a str,
    pub value: &'a [u8],
}

impl<'a> NodeProperty<'a> {
    fn parse(stream: &mut FdtData<'a>, header: &LinuxFdt<'a>) -> Self {
        let tag = stream.u32().expect("FDT property tag").get();
        assert_eq!(tag, FDT_PROP, "bad prop, tag: {}", tag);

        let prop = FdtProperty::from_bytes(stream).expect("FDT property");
        let data_len = prop.len.get() as usize;
        let data = &stream.remaining()[..data_len];
        skip_4_aligned(stream, data_len);

        NodeProperty {
            name: header
                .string_at_offset(prop.name_offset.get() as usize)
                .expect("invalid FDT string offset"),
            value: data,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MemoryRegion {
    pub starting_address: *const u8,
    pub size: usize,
}

#[derive(Debug, Clone)]
pub struct RegIter<'a> {
    stream: FdtData<'a>,
    sizes: CellSizes,
}

impl<'a> RegIter<'a> {
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

    fn parent_cell_sizes(self) -> CellSizes {
        if let Some(parent) = self.parent_props {
            return FdtNode {
                name: "",
                header: self.header,
                props: parent,
                parent_props: None,
            }
            .cell_sizes();
        }

        CellSizes::default()
    }
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
                props: stream.remaining(),
                parent_props,
            });
        }
        Some(part) => part,
    };

    stream.skip_nops();
    let parent_props = Some(stream.remaining());

    while stream.peek_u32()?.get() == FDT_PROP {
        let _ = NodeProperty::parse(stream, header);
    }

    while stream.peek_u32()?.get() == FDT_BEGIN_NODE {
        if let Some(node) = find_node(stream, next_part, header, parent_props) {
            return Some(node);
        }
    }

    stream.skip_nops();
    if stream.u32()?.get() != FDT_END_NODE {
        return None;
    }

    None
}

pub(crate) fn skip_current_node<'a>(stream: &mut FdtData<'a>, header: &LinuxFdt<'a>) {
    assert_eq!(stream.u32().unwrap().get(), FDT_BEGIN_NODE, "bad node");

    let unit_name = CStr::new(stream.remaining())
        .expect("unit_name C str")
        .as_str()
        .unwrap();
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

fn skip_4_aligned(stream: &mut FdtData<'_>, len: usize) {
    stream.skip((len + 3) & !0x3);
}

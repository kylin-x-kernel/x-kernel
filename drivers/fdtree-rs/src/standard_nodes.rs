// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2025 WeiKang Guo <guoweikang.kernel@gmail.com
// Copyright (C) 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSE for license details.

//! Standard nodes in the FDT, such as `/chosen`, `/aliases`, `/cpus/cpu*`, and `/memory`

use crate::{
    node::{CellSizes, FdtNode, NodeProperty},
    parsing::{BigEndianU32, BigEndianU64, CStr, FdtData},
    LinuxFdt,
};

/// Represents the root (`/`) node with specific helper methods
#[derive(Debug, Clone, Copy)]
pub struct Root<'b, 'a> {
    pub(crate) node: FdtNode<'b, 'a>,
}

impl<'b, 'a> Root<'b, 'a> {
    /// Root node cell sizes
    pub fn cell_sizes(self) -> CellSizes {
        self.node.cell_sizes()
    }

    /// `model` property
    pub fn model(self) -> Option<&'a str> {
        self.node
            .properties()
            .find(|p| p.name == "model")
            .and_then(|p| core::str::from_utf8(p.value).ok())
            .map(|s| s.trim_end_matches('\0'))
    }

    /// `compatible` property
    pub fn compatible(self) -> Option<Compatible<'a>> {
        self.node.compatible()
    }

    /// Returns an iterator over all of the available properties
    pub fn properties(self) -> impl Iterator<Item = NodeProperty<'a>> + 'b {
        self.node.properties()
    }

    /// Attempts to find the a property by its name
    pub fn property(self, name: &str) -> Option<NodeProperty<'a>> {
        self.node.properties().find(|p| p.name == name)
    }
}

/// Represents the `/aliases` node with specific helper methods
#[derive(Debug, Clone, Copy)]
pub struct Aliases<'b, 'a> {
    pub(crate) header: &'b LinuxFdt<'a>,
    pub(crate) node: FdtNode<'b, 'a>,
}

impl<'b, 'a> Aliases<'b, 'a> {
    /// Attempt to resolve an alias to a node name
    pub fn resolve(self, alias: &str) -> Option<&'a str> {
        self.node
            .properties()
            .find(|p| p.name == alias)
            .and_then(|p| core::str::from_utf8(p.value).map(|s| s.trim_end_matches('\0')).ok())
    }

    /// Attempt to find the node specified by the given alias
    pub fn resolve_node(self, alias: &str) -> Option<FdtNode<'b, 'a>> {
        self.resolve(alias).and_then(|name| self.header.find_node(name))
    }

    /// Returns an iterator over all of the available aliases
    pub fn all(self) -> impl Iterator<Item = (&'a str, &'a str)> + 'b {
        self.node.properties().filter_map(|p| {
            Some((p.name, core::str::from_utf8(p.value).map(|s| s.trim_end_matches('\0')).ok()?))
        })
    }
}

/// Represents a `/cpus/cpu*` node with specific helper methods
#[derive(Debug, Clone, Copy)]
pub struct Cpu<'b, 'a> {
    pub(crate) parent: FdtNode<'b, 'a>,
    pub(crate) node: FdtNode<'b, 'a>,
}

impl<'b, 'a> Cpu<'b, 'a> {
    /// Return the IDs for the given CPU
    pub fn ids(self) -> Option<CpuIds<'a>> {
        let address_cells = self.node.parent_cell_sizes().address_cells;
        let reg = self.node.properties().find(|p| p.name == "reg")?;
        Some(CpuIds {
            reg,
            address_cells,
        })
    }

    /// `clock-frequency` property
    pub fn clock_frequency(self) -> Option<usize> {
        self.node
            .properties()
            .find(|p| p.name == "clock-frequency")
            .or_else(|| self.parent.property("clock-frequency"))
            .and_then(|p| match p.value.len() {
                4 => BigEndianU32::from_bytes(p.value).map(|v| v.get() as usize),
                8 => BigEndianU64::from_bytes(p.value).map(|v| v.get() as usize),
                _ => None,
            })
    }

    /// `timebase-frequency` property
    pub fn timebase_frequency(self) -> Option<usize> {
        self.node
            .properties()
            .find(|p| p.name == "timebase-frequency")
            .or_else(|| self.parent.property("timebase-frequency"))
            .and_then(|p| match p.value.len() {
                4 => BigEndianU32::from_bytes(p.value).map(|v| v.get() as usize),
                8 => BigEndianU64::from_bytes(p.value).map(|v| v.get() as usize),
                _ => None,
            })
    }

    /// Returns an iterator over all of the properties for the CPU node
    pub fn properties(self) -> impl Iterator<Item = NodeProperty<'a>> + 'b {
        self.node.properties()
    }

    /// Attempts to find the a property by its name
    pub fn property(self, name: &str) -> Option<NodeProperty<'a>> {
        self.node.properties().find(|p| p.name == name)
    }
}

/// Represents the value of the `reg` property of a `/cpus/cpu*` node which may
/// contain more than one CPU or thread ID
#[derive(Debug, Clone, Copy)]
pub struct CpuIds<'a> {
    pub(crate) reg: NodeProperty<'a>,
    pub(crate) address_cells: usize,
}

impl<'a> CpuIds<'a> {
    /// The first listed CPU ID, returns None if parsing fails
    pub fn first(self) -> Option<usize> {
        match self.address_cells {
            1 => BigEndianU32::from_bytes(self.reg.value).map(|v| v.get() as usize),
            2 => BigEndianU64::from_bytes(self.reg.value).map(|v| v.get() as usize),
            _ => None,
        }
    }

    /// Returns an iterator over all of the listed CPU IDs
    pub fn all(self) -> impl Iterator<Item = usize> + 'a {
        let mut vals = FdtData::new(self.reg.value);
        let address_cells = self.address_cells;
        core::iter::from_fn(move || match vals.remaining() {
            [] => None,
            _ => match address_cells {
                1 => vals.u32().map(|v| v.get() as usize),
                2 => vals.u64().map(|v| v.get() as usize),
                _ => None,
            },
        })
    }
}

/// Represents the `compatible` property of a node
#[derive(Clone, Copy)]
pub struct Compatible<'a> {
    pub(crate) data: &'a [u8],
}

impl<'a> Compatible<'a> {
    /// First compatible string, returns None if parsing fails
    pub fn first(self) -> Option<&'a str> {
        CStr::new(self.data)?.as_str()
    }

    /// Returns an iterator over all available compatible strings
    pub fn all(self) -> impl Iterator<Item = &'a str> {
        let mut data = self.data;
        core::iter::from_fn(move || {
            if data.is_empty() {
                return None;
            }

            match data.iter().position(|b| *b == b'\0') {
                Some(idx) => {
                    let ret = core::str::from_utf8(&data[..idx]).ok();
                    data = &data[idx + 1..];
                    ret
                }
                None => {
                    let ret = core::str::from_utf8(data).ok();
                    data = &[];
                    ret
                }
            }
        })
    }
}

/// A memory region
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MemoryRegion {
    /// Starting address represented as a pointer
    pub starting_address: *const u8,
    /// Size of the memory region
    pub size: usize,
}


/// An iterator over the `reg` property of a node
#[derive(Debug, Clone)]
pub struct RegIter<'a> {
    stream: FdtData<'a>,
    sizes: CellSizes,
}

impl<'a> RegIter<'a> {
    /// Create a new `RegIter`
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

        Some(MemoryRegion { starting_address: base, size })
    }
}


// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2025 WeiKang Guo <guoweikang.kernel@gmail.com
// Copyright (C) 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSE for license details.

//! Linux kernel memory nodes


use crate::{
    node::FdtNode,
    parsing::FdtData,
    standard_nodes::RegIter,
};

/// Represents the `device_type="memory"` node with specific helper methods
#[derive(Debug, Clone, Copy)]
pub struct Memory<'b, 'a> {
    pub(crate) node: FdtNode<'b, 'a>,
}

impl<'b, 'a: 'b> Memory<'b, 'a> {
    /// Returns an iterator over all of the available memory regions
    pub fn regions(&self) -> Option<RegIter<'a>> {
        if let Some(usable_mem) = self.node.property("linux,usable-memory") {
            let sizes = self.node.parent_cell_sizes();
            usable_mem.as_reg(sizes)
        } else {
            self.node.reg()
        }
    }

    /// Returns the initial mapped area, if it exists
    pub fn initial_mapped_area(&self) -> Option<MappedArea> {
        let init_mapped_area = self.node.property("initial_mapped_area")?;
        let mut stream = FdtData::new(init_mapped_area.value);

        let effective_address = stream.u64()?.get() as usize;
        let physical_address = stream.u64()?.get() as usize;
        let size = stream.u32()?.get() as usize;

        Some(MappedArea {
            effective_address,
            physical_address,
            size,
        })
    }
}

/// An area described by the `initial-mapped-area` property of the `/memory`
/// node
#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(C)]
pub struct MappedArea {
    /// Effective address of the mapped area
    pub effective_address: usize,
    /// Physical address of the mapped area
    pub physical_address: usize,
    /// Size of the mapped area
    pub size: usize,
}


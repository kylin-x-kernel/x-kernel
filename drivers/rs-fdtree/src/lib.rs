// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! A minimal #![no_std] parser for Linux Flattened Devicetrees.

#![no_std]
#![allow(rustdoc::bare_urls)]

mod error;
mod header;
mod kernel_nodes;
mod node;
mod parsing;

pub use error::FdtError;
use header::FdtHeader;
pub use kernel_nodes::{Dice, InterruptController};
pub use node::{FdtNode, MemoryRegion, NodeProperty, RegIter};
use parsing::{BigEndianU64, CStr, FdtData};

#[derive(Debug, Clone, Copy)]
pub struct MemReserveIter<'a> {
    stream: FdtData<'a>,
}

/// A flattened devicetree located somewhere in memory
#[derive(Clone, Copy)]
pub struct LinuxFdt<'a> {
    data: &'a [u8],
    header: FdtHeader,
}

impl core::fmt::Debug for LinuxFdt<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LinuxFdt")
            .field("total_size", &self.total_size())
            .finish()
    }
}

impl<'a> LinuxFdt<'a> {
    /// Construct a new `Fdt` from a byte buffer
    ///
    /// Note: this function does ***not*** require that the data be 4-byte
    /// aligned
    pub fn new(data: &'a [u8]) -> Result<Self, FdtError> {
        let mut stream = FdtData::new(data);
        let header = FdtHeader::from_bytes(&mut stream).ok_or(FdtError::BufferTooSmall)?;

        if !header.valid_magic() {
            return Err(FdtError::BadMagic);
        } else if data.len() < header.totalsize.get() as usize {
            return Err(FdtError::BufferTooSmall);
        }

        Ok(Self { data, header })
    }

    /// # Safety
    /// This function performs a read to verify the magic value. If the pointer
    /// is invalid this can result in undefined behavior.
    ///
    /// Note: this function does ***not*** require that the data be 4-byte
    /// aligned
    pub unsafe fn from_ptr(ptr: *const u8) -> Result<Self, FdtError> {
        if ptr.is_null() {
            return Err(FdtError::BadPtr);
        }

        // SAFETY: we assume that the pointer is valid and points to a valid FDT
        let tmp_header =
            unsafe { core::slice::from_raw_parts(ptr, core::mem::size_of::<FdtHeader>()) };

        let real_size = FdtHeader::from_bytes(&mut FdtData::new(tmp_header))
            .unwrap()
            .totalsize
            .get() as usize;

        unsafe { Self::new(core::slice::from_raw_parts(ptr, real_size)) }
    }

    /// Total size of the devicetree in bytes
    pub fn total_size(&self) -> usize {
        self.header.totalsize.get() as usize
    }

    /// Returns interrupt controller node.
    ///
    /// Searches for the first node with an "interrupt-controller" property.
    /// Returns `None` if no interrupt controller is found.
    pub fn interrupt_controller(&self) -> Option<InterruptController<'_, 'a>> {
        let ic_node = self
            .all_nodes()
            .find(|node| node.property("interrupt-controller").is_some())?;
        Some(InterruptController { node: ic_node })
    }

    /// Returns the `/chosen/dice` node if present.
    pub fn dice(&self) -> Option<Dice<'_, 'a>> {
        node::find_node(
            &mut FdtData::new(self.structs_block()),
            "/chosen/dice",
            self,
            None,
        )
        .map(|node| Dice { node })
    }

    /// Returns an iterator over all of the nodes in the devicetree, depth-first
    pub fn all_nodes(&self) -> impl Iterator<Item = node::FdtNode<'_, 'a>> {
        node::all_nodes(self)
    }

    pub fn mem_reservations(&self) -> MemReserveIter<'a> {
        MemReserveIter {
            stream: FdtData::new(self.mem_rsvmap_block()),
        }
    }

    pub fn reserved_memory_regions(&self) -> impl Iterator<Item = MemoryRegion> + '_ {
        node::reserved_memory_regions(self)
    }

    fn structs_block(&self) -> &'a [u8] {
        &self.data[self.header.struct_range()]
    }

    fn mem_rsvmap_block(&self) -> &'a [u8] {
        &self.data[self.header.mem_rsvmap_range()]
    }

    pub(crate) fn string_at_offset(&self, offset: usize) -> Option<&'a str> {
        CStr::new(self.strings_block().get(offset..)?)?.as_str()
    }

    fn strings_block(&self) -> &'a [u8] {
        &self.data[self.header.strings_range()]
    }
}

impl<'a> Iterator for MemReserveIter<'a> {
    type Item = MemoryRegion;

    fn next(&mut self) -> Option<Self::Item> {
        let address = BigEndianU64::from_bytes(self.stream.remaining())?.get() as usize;
        self.stream.skip(8);
        let size = BigEndianU64::from_bytes(self.stream.remaining())?.get() as usize;
        self.stream.skip(8);
        if address == 0 && size == 0 {
            return None;
        }
        Some(MemoryRegion {
            starting_address: address as *const u8,
            size,
        })
    }
}

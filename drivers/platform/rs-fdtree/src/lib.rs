// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! A minimal #![no_std] parser for Linux Flattened Devicetrees.

#![no_std]

mod error;
mod header;
mod kernel_nodes;
mod node;
mod parsing;

pub use error::FdtError;
use header::FdtHeader;
pub use kernel_nodes::{Chosen, Dice, InterruptController};
pub use node::{FdtNode, MemoryRegion, NodeProperty, RegIter};
use parsing::{BigEndianU64, CStr, FdtData};

/// Iterator over the DTB memory-reservation block, yielding each
/// `(address, size)` pair until the `0, 0` terminator.
#[derive(Debug, Clone, Copy)]
pub struct MemReserveIter<'a> {
    stream: FdtData<'a>,
}

/// A flattened devicetree located somewhere in memory
#[derive(Clone, Copy)]
pub struct LinuxFdt<'a> {
    data: &'a [u8],
    header: FdtHeader,
    mem_rsvmap_end: usize,
}

impl core::fmt::Debug for LinuxFdt<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LinuxFdt")
            .field("total_size", &self.total_size())
            .finish()
    }
}

impl<'a> LinuxFdt<'a> {
    /// Validates a DTB and borrows its immutable bytes.
    ///
    /// # Errors
    ///
    /// Returns [`FdtError::BadMagic`] for a bad magic value and
    /// [`FdtError::BufferTooSmall`] for missing header/blob storage.
    /// [`FdtError::ParseError`] reports invalid block ranges, missing reservation
    /// termination, unsupported format versions, malformed tokens/properties or
    /// names, and nesting beyond 63 nodes (including the root). No tree is
    /// published until the entire structure has passed validation.
    ///
    /// Note: this function does ***not*** require that the data be 4-byte
    /// aligned
    pub fn new(data: &'a [u8]) -> Result<Self, FdtError> {
        let mut stream = FdtData::new(data);
        let header = FdtHeader::from_bytes(&mut stream).ok_or(FdtError::BufferTooSmall)?;

        if !header.valid_magic() {
            return Err(FdtError::BadMagic);
        }

        let total_size = header.totalsize.get() as usize;
        if data.len() < total_size {
            return Err(FdtError::BufferTooSmall);
        }

        let data = &data[..total_size];
        let mem_rsvmap_end = header.validate(data)?;
        node::validate_structure(&data[header.struct_range()], &data[header.strings_range()])?;
        Ok(Self {
            data,
            header,
            mem_rsvmap_end,
        })
    }

    /// Constructs a `LinuxFdt` from a raw pointer to a DTB blob in
    /// memory.
    ///
    /// # Errors
    ///
    /// Returns [`FdtError::BadPtr`] for a null pointer, otherwise the same
    /// validation errors as [`Self::new`]. Actual readable storage shorter than
    /// the header-declared size violates the caller's safety obligation.
    ///
    /// # Safety
    ///
    /// `ptr` must point to a readable flattened devicetree blob whose header
    /// and total-size range remain readable and immutable for the returned lifetime.
    ///
    /// Note: this function does ***not*** require that the data be 4-byte
    /// aligned
    pub unsafe fn from_ptr(ptr: *const u8) -> Result<Self, FdtError> {
        if ptr.is_null() {
            return Err(FdtError::BadPtr);
        }

        // SAFETY: the caller guarantees a readable header at `ptr`; byte reads
        // do not require FdtHeader alignment or structurally valid contents.
        let tmp_header =
            unsafe { core::slice::from_raw_parts(ptr, core::mem::size_of::<FdtHeader>()) };

        #[expect(
            clippy::unwrap_used,
            reason = "CG-021-no-panic-on-reachable-failure: tmp_header contains \
                      size_of::<FdtHeader>() bytes and the parser only consumes its ten u32 fields"
        )]
        let real_size = FdtHeader::from_bytes(&mut FdtData::new(tmp_header))
            .unwrap()
            .totalsize
            .get() as usize;

        // SAFETY: the caller guarantees that the full header-declared range is
        // readable and immutable for the returned lifetime. Structural
        // validation follows inside `new`; it cannot establish pointer validity.
        unsafe { Self::new(core::slice::from_raw_parts(ptr, real_size)) }
    }

    /// Total size of the devicetree in bytes
    pub fn total_size(&self) -> usize {
        self.header.totalsize.get() as usize
    }

    /// Returns the complete flattened device-tree blob.
    pub fn as_bytes(&self) -> &'a [u8] {
        &self.data[..self.total_size()]
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

    /// Returns the `/chosen` node if present.
    pub fn chosen(&self) -> Option<Chosen<'_, 'a>> {
        self.find_node("/chosen").map(|node| Chosen { node })
    }

    /// Returns the Open DICE node under `/reserved-memory` if present.
    pub fn dice(&self) -> Option<Dice<'_, 'a>> {
        self.find_node("/reserved-memory/dice")
            .or_else(|| {
                self.find_compatible("google,open-dice")
                    .or_else(|| self.find_compatible("kylin,open-dice"))
            })
            .map(|node| Dice { node })
    }

    /// Returns an iterator over all of the nodes in the devicetree, depth-first
    pub fn all_nodes(&self) -> impl Iterator<Item = node::FdtNode<'_, 'a>> {
        node::all_nodes(self)
    }

    /// Finds a node by an absolute device-tree path.
    pub fn find_node(&self, path: &str) -> Option<node::FdtNode<'_, 'a>> {
        node::find_node(
            &mut parsing::FdtData::new(self.structs_block()),
            path,
            self,
            None,
        )
    }

    /// Iterates the memreserve block entries (physical address/size pairs).
    pub fn mem_reservations(&self) -> MemReserveIter<'a> {
        MemReserveIter {
            stream: FdtData::new(self.mem_rsvmap_block()),
        }
    }

    /// Iterates `/memory` node regions as [`MemoryRegion`]s.
    pub fn memory_regions(&self) -> impl Iterator<Item = MemoryRegion> + '_ {
        node::memory_regions(self)
    }

    /// Iterates `/reserved-memory` child regions as [`MemoryRegion`]s.
    pub fn reserved_memory_regions(&self) -> impl Iterator<Item = MemoryRegion> + '_ {
        node::reserved_memory_regions(self)
    }

    /// Iterates `/reserved-memory` child nodes.
    pub fn reserved_memory_nodes(&self) -> impl Iterator<Item = node::FdtNode<'_, 'a>> + '_ {
        node::reserved_memory_nodes(self)
    }

    /// Returns the boot arguments from the `/chosen` node.
    pub fn chosen_bootargs(&self) -> Option<&'a str> {
        self.chosen()?.bootargs()
    }

    /// Returns the console device path from the `/chosen` node.
    pub fn chosen_stdout_path(&self) -> Option<&'a str> {
        self.chosen()?.stdout_path()
    }

    /// Returns the root node's `model` string.
    pub fn root_model(&self) -> Option<&'a str> {
        self.find_node("/")?.property_str("model")
    }

    /// Returns the root node's first `compatible` string.
    pub fn root_compatible(&self) -> Option<&'a str> {
        self.find_node("/")?.compatible()
    }

    /// Finds the first node whose `compatible` list contains the string.
    pub fn find_compatible(&self, compatible: &str) -> Option<node::FdtNode<'_, 'a>> {
        self.all_nodes().find(|node| node.is_compatible(compatible))
    }

    /// Resolves a path or an alias (looked up under `/aliases`) to a node;
    /// a `:`-suffixed cell suffix is ignored.
    pub fn resolve_node(&self, path_or_alias: &str) -> Option<node::FdtNode<'_, 'a>> {
        let key = path_or_alias.split(':').next()?;
        if key.starts_with('/') {
            return self.find_node(key);
        }

        let alias_value = self.find_node("/aliases")?.property_str(key)?;
        self.find_node(alias_value.split(':').next()?)
    }

    fn structs_block(&self) -> &'a [u8] {
        &self.data[self.header.struct_range()]
    }

    fn mem_rsvmap_block(&self) -> &'a [u8] {
        &self.data[self.header.mem_rsvmap_range().start..self.mem_rsvmap_end]
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

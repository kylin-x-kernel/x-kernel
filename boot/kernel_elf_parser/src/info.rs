// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ELF information parsed from the ELF file

use alloc::vec::Vec;
use core::{mem::size_of, ops::Range};

use xmas_elf::{
    header::Class,
    program::{ProgramHeader32, ProgramHeader64},
};

use crate::auxv::{AuxEntry, AuxType};

pub struct ElfHeadersBuilder<'a>(ElfHeaders<'a>);
impl<'a> ElfHeadersBuilder<'a> {
    pub fn new(input: &'a [u8]) -> Result<Self, &'static str> {
        Ok(Self(ElfHeaders {
            header: xmas_elf::header::parse_header(input)?,
            ph: Vec::new(),
        }))
    }

    /// Returns the byte range occupied by the program-header table.
    ///
    /// The range is derived from untrusted ELF fields and is therefore checked
    /// for multiplication and addition overflow before it is returned.
    ///
    /// # Errors
    ///
    /// Returns an error when the entry size does not match the ELF class or
    /// when the table range cannot be represented as a `u64` range.
    pub fn ph_range(&self) -> Result<Range<u64>, &'static str> {
        let start = self.0.header.pt2.ph_offset();
        let entry_size = u64::from(self.0.header.pt2.ph_entry_size());
        let expected_entry_size = match self.0.header.pt1.class() {
            Class::ThirtyTwo => size_of::<ProgramHeader32>() as u64,
            Class::SixtyFour => size_of::<ProgramHeader64>() as u64,
            Class::None | Class::Other(_) => return Err("Invalid ELF class"),
        };
        if entry_size != expected_entry_size {
            return Err("Invalid program-header entry size");
        }
        let count = u64::from(self.0.header.pt2.ph_count());
        let size = entry_size
            .checked_mul(count)
            .ok_or("Program-header table size overflows")?;
        let end = start
            .checked_add(size)
            .ok_or("Program-header table range overflows")?;
        Ok(start..end)
    }

    /// Builds the parsed ELF headers from the complete program-header table.
    ///
    /// `ph` must contain exactly the byte range returned by [`Self::ph_range`].
    ///
    /// # Errors
    ///
    /// Returns an error when the ELF class is unsupported, the program-header
    /// entry size or table length is inconsistent with the ELF header, or a
    /// program-header entry cannot be parsed.
    pub fn build(mut self, ph: &[u8]) -> Result<ElfHeaders<'a>, &'static str> {
        let entry_size = usize::from(self.0.header.pt2.ph_entry_size());
        let expected_entry_size = match self.0.header.pt1.class() {
            Class::ThirtyTwo => size_of::<ProgramHeader32>(),
            Class::SixtyFour => size_of::<ProgramHeader64>(),
            Class::None | Class::Other(_) => return Err("Invalid ELF class"),
        };
        if entry_size != expected_entry_size {
            return Err("Invalid program-header entry size");
        }
        let expected_len = entry_size
            .checked_mul(usize::from(self.0.header.pt2.ph_count()))
            .ok_or("Program-header table size overflows")?;
        if ph.len() != expected_len {
            return Err("Incomplete program-header table");
        }

        self.0.ph = ph
            .chunks_exact(entry_size)
            .map(|chunk| match self.0.header.pt1.class() {
                Class::ThirtyTwo => {
                    let ph: &ProgramHeader32 = zero::read(chunk);
                    ProgramHeader64 {
                        type_: ph.type_,
                        offset: ph.offset as _,
                        virtual_addr: ph.virtual_addr as _,
                        physical_addr: ph.physical_addr as _,
                        file_size: ph.file_size as _,
                        mem_size: ph.mem_size as _,
                        flags: ph.flags,
                        align: ph.align as _,
                    }
                }
                Class::SixtyFour => *zero::read(chunk),
                Class::None | Class::Other(_) => unreachable!(),
            })
            .collect();
        Ok(self.0)
    }
}

pub struct ElfHeaders<'a> {
    pub header: xmas_elf::header::Header<'a>,
    pub ph: Vec<ProgramHeader64>,
}

/// A wrapper for the ELF file data with some useful methods.
pub struct ElfParser<'a> {
    headers: &'a ElfHeaders<'a>,
    /// Base address of the ELF file loaded into the memory.
    base: usize,
    /// Validated virtual address of the program-header table.
    phdr: usize,
}

impl<'a> ElfParser<'a> {
    /// Creates a parser with a validated program-header address.
    ///
    /// # Errors
    ///
    /// Returns an error if no segment covers the program-header table offset
    /// or its biased virtual address cannot be represented.
    pub fn new(headers: &'a ElfHeaders<'a>, bias: usize) -> Result<Self, &'static str> {
        let base = if headers.header.pt2.type_().as_type() == xmas_elf::header::Type::SharedObject {
            bias
        } else {
            0
        };
        let ph_offset = headers.header.pt2.ph_offset();
        let header = headers
            .ph
            .iter()
            .find(|header| {
                header.offset <= ph_offset && ph_offset - header.offset < header.file_size
            })
            .ok_or("program-header table is not covered by a segment")?;
        let phdr = header
            .virtual_addr
            .checked_add(ph_offset - header.offset)
            .and_then(|address| usize::try_from(address).ok())
            .and_then(|address| address.checked_add(base))
            .ok_or("program-header address overflows")?;
        Ok(Self {
            headers,
            base,
            phdr,
        })
    }

    /// The entry point of the ELF file.
    pub fn entry(&self) -> usize {
        // TODO: base_load_address_offset?
        self.headers.header.pt2.entry_point() as usize + self.base
    }

    /// The number of program headers in the ELF file.
    pub fn phnum(&self) -> usize {
        self.headers.header.pt2.ph_count() as usize
    }

    /// The size of the program header table entry in the ELF file.
    pub fn phent(&self) -> usize {
        self.headers.header.pt2.ph_entry_size() as usize
    }

    /// The validated virtual address of the program-header table.
    pub fn phdr(&self) -> usize {
        self.phdr
    }

    /// The base address of the ELF file loaded into the memory.
    pub fn base(&self) -> usize {
        self.base
    }

    pub fn headers(&self) -> &'a ElfHeaders<'a> {
        self.headers
    }

    /// Part of auxiliary vectors from the ELF file.
    ///
    /// # Arguments
    ///
    /// * `pagesz` - The page size of the system
    /// * `ldso_base` - The base address of the dynamic linker (if exists)
    ///
    /// Details about auxiliary vectors are described in <https://articles.manugarg.com/aboutelfauxiliaryvectors.html>
    pub fn aux_vector(
        &self,
        pagesz: usize,
        ldso_base: Option<usize>,
    ) -> impl Iterator<Item = AuxEntry> {
        [
            (AuxType::PHDR, self.phdr()),
            (AuxType::PHENT, self.phent()),
            (AuxType::PHNUM, self.phnum()),
            (AuxType::PAGESZ, pagesz),
            (AuxType::ENTRY, self.entry()),
        ]
        .into_iter()
        .chain(ldso_base.into_iter().map(|base| (AuxType::BASE, base)))
        .map(|(at, val)| AuxEntry::new(at, val))
    }
}

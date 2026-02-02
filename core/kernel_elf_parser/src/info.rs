//! ELF information parsed from the ELF file

use alloc::vec::Vec;
use core::ops::Range;

use xmas_elf::{
    header::Class,
    program::{ProgramHeader32, ProgramHeader64},
};

use crate::auxv::{AuxEntry, AuxType};

pub struct ELFHeadersBuilder<'a>(ELFHeaders<'a>);
impl<'a> ELFHeadersBuilder<'a> {
    pub fn new(input: &'a [u8]) -> Result<Self, &'static str> {
        Ok(Self(ELFHeaders {
            header: xmas_elf::header::parse_header(input)?,
            ph: Vec::new(),
        }))
    }

    pub fn ph_range(&self) -> Range<u64> {
        let start = self.0.header.pt2.ph_offset();
        let size = self.0.header.pt2.ph_entry_size() as u64 * self.0.header.pt2.ph_count() as u64;
        start..start + size
    }

    pub fn build(mut self, ph: &[u8]) -> Result<ELFHeaders<'a>, &'static str> {
        self.0.ph = ph
            .chunks_exact(self.0.header.pt2.ph_entry_size() as usize)
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

pub struct ELFHeaders<'a> {
    pub header: xmas_elf::header::Header<'a>,
    pub ph: Vec<ProgramHeader64>,
}

/// A wrapper for the ELF file data with some useful methods.
pub struct ELFParser<'a> {
    headers: &'a ELFHeaders<'a>,
    /// Base address of the ELF file loaded into the memory.
    base: usize,
}

impl<'a> ELFParser<'a> {
    /// Create a new `ELFInfo` instance.
    pub fn new(headers: &'a ELFHeaders<'a>, bias: usize) -> Result<Self, &'static str> {
        let base = if headers.header.pt2.type_().as_type() == xmas_elf::header::Type::SharedObject {
            bias
        } else {
            0
        };
        Ok(Self { headers, base })
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

    /// The offset of the program header table in the ELF file.
    pub fn phdr(&self) -> usize {
        let ph_offset = self.headers.header.pt2.ph_offset() as usize;
        let header = self
            .headers
            .ph
            .iter()
            .find(|header| {
                (header.offset..header.offset + header.file_size).contains(&(ph_offset as u64))
            })
            .expect("can not find program header table address in elf");
        ph_offset - header.offset as usize + header.virtual_addr as usize + self.base
    }

    /// The base address of the ELF file loaded into the memory.
    pub fn base(&self) -> usize {
        self.base
    }

    pub fn headers(&self) -> &'a ELFHeaders<'a> {
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

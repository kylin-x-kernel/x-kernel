// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ELF information parsed from the ELF file

use alloc::vec::Vec;
use core::{mem::size_of, ops::Range, ptr};

use xmas_elf::{
    P32, P64,
    header::{Class, HeaderPt1, HeaderPt2_},
    program::{ProgramHeader32, ProgramHeader64},
};

use crate::{
    ElfError,
    auxv::{AuxEntry, AuxType},
};

pub struct ElfHeadersBuilder<'a>(ElfHeaders<'a>);
impl<'a> ElfHeadersBuilder<'a> {
    /// Parses the ELF file header at the start of `input`.
    ///
    /// The parsed header borrows `input` in place, so the start of `input`
    /// must be aligned for the class-specific header part: 4 bytes for
    /// ELF32 and 8 bytes for ELF64. A misaligned buffer is reported as an
    /// error rather than parsed.
    ///
    /// # Errors
    ///
    /// Returns an error when `input` is shorter than the ELF header, lacks the
    /// ELF magic, has an unsupported class, or is not aligned as required.
    pub fn new(input: &'a [u8]) -> Result<Self, ElfError> {
        check_header_alignment(input)?;
        Ok(Self(ElfHeaders {
            header: xmas_elf::header::parse_header(input).map_err(ElfError::InvalidHeader)?,
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
    pub fn ph_range(&self) -> Result<Range<u64>, ElfError> {
        let start = self.0.header.pt2.ph_offset();
        let entry_size = u64::from(self.0.header.pt2.ph_entry_size());
        let expected_entry_size = match self.0.header.pt1.class() {
            Class::ThirtyTwo => size_of::<ProgramHeader32>() as u64,
            Class::SixtyFour => size_of::<ProgramHeader64>() as u64,
            Class::None | Class::Other(_) => return Err(ElfError::InvalidClass),
        };
        if entry_size != expected_entry_size {
            return Err(ElfError::InvalidProgramHeaderEntrySize);
        }
        let count = u64::from(self.0.header.pt2.ph_count());
        let size = entry_size
            .checked_mul(count)
            .ok_or(ElfError::ProgramHeaderTableSizeOverflow)?;
        let end = start
            .checked_add(size)
            .ok_or(ElfError::ProgramHeaderTableRangeOverflow)?;
        Ok(start..end)
    }

    /// Builds the parsed ELF headers from the complete program-header table.
    ///
    /// `ph` must contain exactly the byte range returned by [`Self::ph_range`].
    /// It may start at any byte offset: entries are copied out by value, so an
    /// `e_phoff` that is not a multiple of the entry alignment is accepted.
    ///
    /// # Errors
    ///
    /// Returns an error when the ELF class is unsupported, the program-header
    /// entry size or table length is inconsistent with the ELF header, or a
    /// program-header entry cannot be parsed.
    pub fn build(mut self, ph: &[u8]) -> Result<ElfHeaders<'a>, ElfError> {
        let entry_size = usize::from(self.0.header.pt2.ph_entry_size());
        let expected_entry_size = match self.0.header.pt1.class() {
            Class::ThirtyTwo => size_of::<ProgramHeader32>(),
            Class::SixtyFour => size_of::<ProgramHeader64>(),
            Class::None | Class::Other(_) => return Err(ElfError::InvalidClass),
        };
        if entry_size != expected_entry_size {
            return Err(ElfError::InvalidProgramHeaderEntrySize);
        }
        let expected_len = entry_size
            .checked_mul(usize::from(self.0.header.pt2.ph_count()))
            .ok_or(ElfError::ProgramHeaderTableSizeOverflow)?;
        if ph.len() != expected_len {
            return Err(ElfError::IncompleteProgramHeaderTable);
        }

        self.0.ph = match self.0.header.pt1.class() {
            Class::ThirtyTwo => ph
                .chunks_exact(entry_size)
                .map(|entry| read_program_header32(entry).map(widen_program_header))
                .collect::<Result<_, _>>()?,
            Class::SixtyFour => ph
                .chunks_exact(entry_size)
                .map(read_program_header64)
                .collect::<Result<_, _>>()?,
            Class::None | Class::Other(_) => return Err(ElfError::InvalidClass),
        };
        Ok(self.0)
    }
}

/// Byte offset of `EI_CLASS` in the ELF identification.
const EI_CLASS: usize = 4;
const ELFCLASS32: u8 = 1;
const ELFCLASS64: u8 = 2;

/// Rejects a header buffer that `xmas_elf::header::parse_header` would borrow
/// at a misaligned address.
///
/// `parse_header` returns `&HeaderPt2_` into `input` through `zero::read`,
/// which asserts alignment instead of returning an error. Inputs that are too
/// short or carry an unknown class are left to `parse_header` to reject.
fn check_header_alignment(input: &[u8]) -> Result<(), ElfError> {
    let Some(pt2) = input.get(size_of::<HeaderPt1>()..) else {
        return Ok(());
    };
    let is_aligned = match input.get(EI_CLASS).copied() {
        Some(ELFCLASS32) => pt2.as_ptr().cast::<HeaderPt2_<P32>>().is_aligned(),
        Some(ELFCLASS64) => pt2.as_ptr().cast::<HeaderPt2_<P64>>().is_aligned(),
        _ => true,
    };
    if is_aligned {
        Ok(())
    } else {
        Err(ElfError::MisalignedHeader)
    }
}

/// Copies one ELF32 program-header entry that may start at any byte offset.
fn read_program_header32(entry: &[u8]) -> Result<ProgramHeader32, ElfError> {
    let entry: &[u8; size_of::<ProgramHeader32>()] = entry
        .try_into()
        .map_err(|_| ElfError::TruncatedProgramHeaderEntry)?;
    // SAFETY: `entry` is exactly `size_of::<ProgramHeader32>()` readable,
    // initialized bytes. `xmas_elf` implements `zero::Pod` for this `repr(C)`
    // type of integer fields, so every byte pattern is a valid value, and
    // `read_unaligned` places no alignment requirement on the source.
    Ok(unsafe { ptr::read_unaligned(entry.as_ptr().cast::<ProgramHeader32>()) })
}

/// Copies one ELF64 program-header entry that may start at any byte offset.
fn read_program_header64(entry: &[u8]) -> Result<ProgramHeader64, ElfError> {
    let entry: &[u8; size_of::<ProgramHeader64>()] = entry
        .try_into()
        .map_err(|_| ElfError::TruncatedProgramHeaderEntry)?;
    // SAFETY: `entry` is exactly `size_of::<ProgramHeader64>()` readable,
    // initialized bytes. `xmas_elf` implements `zero::Pod` for this `repr(C)`
    // type of integer fields, so every byte pattern is a valid value, and
    // `read_unaligned` places no alignment requirement on the source.
    Ok(unsafe { ptr::read_unaligned(entry.as_ptr().cast::<ProgramHeader64>()) })
}

fn widen_program_header(ph: ProgramHeader32) -> ProgramHeader64 {
    ProgramHeader64 {
        type_: ph.type_,
        offset: ph.offset.into(),
        virtual_addr: ph.virtual_addr.into(),
        physical_addr: ph.physical_addr.into(),
        file_size: ph.file_size.into(),
        mem_size: ph.mem_size.into(),
        flags: ph.flags,
        align: ph.align.into(),
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
    pub fn new(headers: &'a ElfHeaders<'a>, bias: usize) -> Result<Self, ElfError> {
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
            .ok_or(ElfError::ProgramHeaderTableUnmapped)?;
        let phdr = header
            .virtual_addr
            .checked_add(ph_offset - header.offset)
            .and_then(|address| usize::try_from(address).ok())
            .and_then(|address| address.checked_add(base))
            .ok_or(ElfError::ProgramHeaderAddressOverflow)?;
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

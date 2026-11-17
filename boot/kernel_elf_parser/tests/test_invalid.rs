// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kernel_elf_parser::{ElfError, ElfHeadersBuilder};

const ELF_HEADER_SIZE: usize = 64;
const PROGRAM_HEADER_SIZE: u16 = 56;
const ELF32_HEADER_SIZE: usize = 52;
const ELF32_PROGRAM_HEADER_SIZE: u16 = 32;
const PT_LOAD: u32 = 1;
const PF_R_X: u32 = 0b101;

/// File-head buffer with the alignment a kernel heap allocation provides, so
/// the only misalignment in a test is the one it introduces on purpose.
#[repr(C, align(8))]
struct FileHead([u8; 256]);

fn elf_header(program_header_offset: u64, entry_size: u16, entry_count: u16) -> [u8; 64] {
    let mut data = [0; ELF_HEADER_SIZE];
    data[0..4].copy_from_slice(b"\x7fELF");
    data[4] = 2;
    data[5] = 1;
    data[6] = 1;
    put_u16(&mut data, 16, 2);
    put_u16(&mut data, 18, 62);
    put_u32(&mut data, 20, 1);
    put_u64(&mut data, 32, program_header_offset);
    put_u16(&mut data, 52, ELF_HEADER_SIZE as u16);
    put_u16(&mut data, 54, entry_size);
    put_u16(&mut data, 56, entry_count);
    data
}

#[test]
fn program_header_range_rejects_end_overflow() {
    let data = elf_header(u64::MAX - 7, PROGRAM_HEADER_SIZE, 1);
    let builder = ElfHeadersBuilder::new(&data).expect("ELF header must parse");

    assert_eq!(
        builder.ph_range(),
        Err(ElfError::ProgramHeaderTableRangeOverflow)
    );
}

#[test]
fn program_header_range_rejects_wrong_entry_size() {
    let data = elf_header(ELF_HEADER_SIZE as u64, PROGRAM_HEADER_SIZE - 1, 1);
    let builder = ElfHeadersBuilder::new(&data).expect("ELF header must parse");

    assert_eq!(
        builder.ph_range(),
        Err(ElfError::InvalidProgramHeaderEntrySize)
    );
}

#[test]
fn build_rejects_incomplete_program_header_table() {
    let data = elf_header(ELF_HEADER_SIZE as u64, PROGRAM_HEADER_SIZE, 1);
    let builder = ElfHeadersBuilder::new(&data).expect("ELF header must parse");

    assert!(builder.build(&[0; 8]).is_err());
}

/// Parses the header and builds the program headers from a sub-slice of the
/// file head, the way `kexec` does when the table lies in the bytes it has
/// already read.
fn build_from_file_head(data: &[u8]) -> Result<Vec<xmas_elf::program::ProgramHeader64>, ElfError> {
    let builder = ElfHeadersBuilder::new(data)?;
    let range = builder.ph_range()?;
    let start =
        usize::try_from(range.start).map_err(|_| ElfError::ProgramHeaderTableRangeOverflow)?;
    let end = usize::try_from(range.end).map_err(|_| ElfError::ProgramHeaderTableRangeOverflow)?;
    builder.build(&data[start..end]).map(|headers| headers.ph)
}

fn elf64_file_head(program_header_offset: usize) -> FileHead {
    let mut head = FileHead([0; 256]);
    let header = elf_header(program_header_offset as u64, PROGRAM_HEADER_SIZE, 1);
    head.0[..ELF_HEADER_SIZE].copy_from_slice(&header);
    let entry = &mut head.0[program_header_offset..];
    put_u32(entry, 0, PT_LOAD);
    put_u32(entry, 4, PF_R_X);
    put_u64(entry, 8, 0);
    put_u64(entry, 16, 0x40_0000);
    put_u64(entry, 24, 0x40_0000);
    put_u64(entry, 32, 0x1234);
    put_u64(entry, 40, 0x5678);
    put_u64(entry, 48, 0x1000);
    head
}

fn elf32_file_head(program_header_offset: usize) -> FileHead {
    let mut head = FileHead([0; 256]);
    let data = &mut head.0;
    data[0..4].copy_from_slice(b"\x7fELF");
    data[4] = 1;
    data[5] = 1;
    data[6] = 1;
    put_u16(data, 16, 2);
    put_u16(data, 18, 3);
    put_u32(data, 20, 1);
    put_u32(data, 28, program_header_offset as u32);
    put_u16(data, 40, ELF32_HEADER_SIZE as u16);
    put_u16(data, 42, ELF32_PROGRAM_HEADER_SIZE);
    put_u16(data, 44, 1);
    let entry = &mut data[program_header_offset..];
    put_u32(entry, 0, PT_LOAD);
    put_u32(entry, 4, 0);
    put_u32(entry, 8, 0x40_0000);
    put_u32(entry, 12, 0x40_0000);
    put_u32(entry, 16, 0x1234);
    put_u32(entry, 20, 0x5678);
    put_u32(entry, 24, PF_R_X);
    put_u32(entry, 28, 0x1000);
    head
}

fn assert_load_segment(ph: &[xmas_elf::program::ProgramHeader64]) {
    assert_eq!(ph.len(), 1);
    assert_eq!(ph[0].get_type(), Ok(xmas_elf::program::Type::Load));
    assert_eq!(ph[0].flags.0, PF_R_X);
    assert_eq!(ph[0].offset, 0);
    assert_eq!(ph[0].virtual_addr, 0x40_0000);
    assert_eq!(ph[0].physical_addr, 0x40_0000);
    assert_eq!(ph[0].file_size, 0x1234);
    assert_eq!(ph[0].mem_size, 0x5678);
    assert_eq!(ph[0].align, 0x1000);
}

#[test]
fn build_reads_aligned_elf64_program_headers() {
    let head = elf64_file_head(64);
    assert_load_segment(&build_from_file_head(&head.0).expect("aligned table must parse"));
}

#[test]
fn build_reads_unaligned_elf64_program_headers() {
    let head = elf64_file_head(65);
    assert_load_segment(&build_from_file_head(&head.0).expect("unaligned table must parse"));
}

#[test]
fn build_reads_unaligned_elf32_program_headers() {
    let head = elf32_file_head(ELF32_HEADER_SIZE + 2);
    assert_load_segment(&build_from_file_head(&head.0).expect("unaligned table must parse"));
}

#[test]
fn new_rejects_misaligned_header_buffer() {
    let head = elf64_file_head(64);
    let mut shifted = FileHead([0; 256]);
    shifted.0[1..].copy_from_slice(&head.0[..255]);

    assert!(ElfHeadersBuilder::new(&shifted.0[1..]).is_err());
}

#[test]
fn new_rejects_truncated_headers() {
    let head = elf64_file_head(64);

    assert!(ElfHeadersBuilder::new(&[]).is_err());
    assert!(ElfHeadersBuilder::new(&head.0[..4]).is_err());
    assert!(ElfHeadersBuilder::new(&head.0[..ELF_HEADER_SIZE - 1]).is_err());
}

fn put_u16(data: &mut [u8], offset: usize, value: u16) {
    data[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(data: &mut [u8], offset: usize, value: u32) {
    data[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(data: &mut [u8], offset: usize, value: u64) {
    data[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

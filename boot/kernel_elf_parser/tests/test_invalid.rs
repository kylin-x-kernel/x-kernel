// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kernel_elf_parser::ELFHeadersBuilder;

const ELF_HEADER_SIZE: usize = 64;
const PROGRAM_HEADER_SIZE: u16 = 56;

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
    let builder = ELFHeadersBuilder::new(&data).expect("ELF header must parse");

    assert_eq!(
        builder.ph_range(),
        Err("Program-header table range overflows")
    );
}

#[test]
fn program_header_range_rejects_wrong_entry_size() {
    let data = elf_header(ELF_HEADER_SIZE as u64, PROGRAM_HEADER_SIZE - 1, 1);
    let builder = ELFHeadersBuilder::new(&data).expect("ELF header must parse");

    assert_eq!(builder.ph_range(), Err("Invalid program-header entry size"));
}

#[test]
fn build_rejects_incomplete_program_header_table() {
    let data = elf_header(ELF_HEADER_SIZE as u64, PROGRAM_HEADER_SIZE, 1);
    let builder = ELFHeadersBuilder::new(&data).expect("ELF header must parse");

    assert!(builder.build(&[0; 8]).is_err());
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

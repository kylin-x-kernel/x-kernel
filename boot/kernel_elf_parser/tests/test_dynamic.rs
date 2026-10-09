// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kernel_elf_parser::ElfHeadersBuilder;

#[test]
fn test_elf_parser() {
    // The parser borrows aligned ELF structures; padding the length alone
    // does not establish pointer alignment.
    #[repr(align(16))]
    struct AlignedElf([u8; include_bytes!("ld-linux-x86-64.so.2").len()]);
    let aligned_image = AlignedElf(*include_bytes!("ld-linux-x86-64.so.2"));
    let aligned_elf_bytes = aligned_image.0.as_slice();

    let builder = ElfHeadersBuilder::new(aligned_elf_bytes).expect("Failed to parse ELF header");
    let range = builder.ph_range().expect("Invalid program-header range");
    let headers = builder
        .build(&aligned_elf_bytes[range.start as usize..range.end as usize])
        .expect("Failed to parse program headers");

    let interp_base = 0x1000;
    let elf_parser = kernel_elf_parser::ElfParser::new(&headers, interp_base).unwrap();
    let base_addr = elf_parser.base();
    assert_eq!(base_addr, interp_base);

    let segments: Vec<_> = elf_parser
        .headers()
        .ph
        .iter()
        .filter(|ph| ph.get_type() == Ok(xmas_elf::program::Type::Load))
        .collect();
    assert_eq!(segments.len(), 4);
    for segment in segments.iter() {
        println!(
            "{:?} flags={:?}",
            segment.virtual_addr,
            xmas_elf::program::Flags::from(segment.flags)
        );
    }
    assert_eq!(segments[0].virtual_addr, 0);
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kernel_elf_parser::{AuxType, ELFHeadersBuilder, ELFParser};

#[test]
fn test_elf_parser() {
    // A simple elf file compiled by the x86_64-linux-musl-gcc.
    let elf_bytes = include_bytes!("elf_static");
    // Ensure the alignment of the byte array
    let mut aligned_elf_bytes = unsafe {
        let ptr = elf_bytes.as_ptr() as *mut u8;
        std::slice::from_raw_parts_mut(ptr, elf_bytes.len())
    }
    .to_vec();
    if aligned_elf_bytes.len() % 16 != 0 {
        let padding = vec![0u8; 16 - aligned_elf_bytes.len() % 16];
        aligned_elf_bytes.extend(padding);
    }

    let builder =
        ELFHeadersBuilder::new(aligned_elf_bytes.as_slice()).expect("Failed to parse ELF header");
    let range = builder.ph_range().expect("Invalid program-header range");
    let headers = builder
        .build(&aligned_elf_bytes[range.start as usize..range.end as usize])
        .expect("Failed to parse program headers");

    let interp_base = 0x1000;
    let elf_parser = kernel_elf_parser::ELFParser::new(&headers, interp_base).unwrap();
    let base_addr = elf_parser.base();
    assert_eq!(base_addr, 0);

    let segments: Vec<_> = elf_parser
        .headers()
        .ph
        .iter()
        .filter(|ph| ph.get_type() == Ok(xmas_elf::program::Type::Load))
        .collect();
    assert_eq!(segments.len(), 4);
    let mut last_start = 0;
    for segment in segments.iter() {
        // start vaddr should be sorted
        assert!(segment.virtual_addr > last_start);
        last_start = segment.virtual_addr;
    }
    assert_eq!(segments[0].virtual_addr, 0x400000);

    test_ustack(&elf_parser);
}

fn test_ustack(elf_parser: &ELFParser) {
    let auxv = elf_parser.aux_vector(0x1000, None).collect::<Vec<_>>();
    // let phent = auxv.get(&AT_PHENT).unwrap();
    // assert_eq!(*phent, 56);
    auxv.iter().for_each(|entry| {
        if entry.get_type() == kernel_elf_parser::AuxType::PHENT {
            assert_eq!(entry.value(), 56);
        }
    });

    let args: Vec<String> = vec!["arg1".to_string(), "arg2".to_string(), "arg3".to_string()];
    let envs: Vec<String> = vec!["LOG=file".to_string()];

    // The highest address of the user stack.
    let ustack_end = 0x4000_0000;

    let random_bytes = [0x5a; 16];
    let stack_data = kernel_elf_parser::app_stack_region(
        &args,
        &envs,
        &auxv,
        ustack_end,
        &random_bytes,
        "/bin/test",
    )
    .unwrap();
    // The first 8 bytes of the stack is the number of arguments.
    assert_eq!(stack_data[0..8], [3, 0, 0, 0, 0, 0, 0, 0]);
}

#[test]
fn initial_stack_supports_empty_argv_and_terminates_auxv_last() {
    let stack_top = 0x4000_0000;
    let random_bytes = [0x5a; 16];
    let terminated_auxv = [kernel_elf_parser::AuxEntry::new(AuxType::NULL, 0)];
    let stack_data = kernel_elf_parser::app_stack_region(
        &[],
        &[],
        &terminated_auxv,
        stack_top,
        &random_bytes,
        "/bin/empty-argv",
    )
    .unwrap();
    let stack_base = stack_top - stack_data.len();

    let read_word = |word_index: usize| {
        let start = word_index * size_of::<usize>();
        usize::from_ne_bytes(
            stack_data[start..start + size_of::<usize>()]
                .try_into()
                .unwrap(),
        )
    };

    assert_eq!(read_word(0), 0);
    assert_eq!(read_word(1), 0);
    assert_eq!(read_word(2), 0);

    let mut aux_word = 3;
    let mut random_address = None;
    let mut execfn_address = None;
    loop {
        let kind = read_word(aux_word);
        let value = read_word(aux_word + 1);
        aux_word += 2;
        if kind == AuxType::NULL as usize {
            break;
        }
        if kind == AuxType::RANDOM as usize {
            random_address = Some(value);
        }
        if kind == AuxType::EXECFN as usize {
            execfn_address = Some(value);
        }
    }

    let random_offset = random_address.unwrap() - stack_base;
    assert_eq!(
        &stack_data[random_offset..random_offset + random_bytes.len()],
        &random_bytes
    );
    let execfn_offset = execfn_address.unwrap() - stack_base;
    assert_eq!(
        &stack_data[execfn_offset..execfn_offset + b"/bin/empty-argv\0".len()],
        b"/bin/empty-argv\0"
    );
}

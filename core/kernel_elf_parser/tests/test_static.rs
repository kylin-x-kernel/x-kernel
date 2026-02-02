use kernel_elf_parser::ELFParser;

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
    let elf =
        xmas_elf::ElfFile::new(aligned_elf_bytes.as_slice()).expect("Failed to read elf file");

    let interp_base = 0x1000;
    let elf_parser = kernel_elf_parser::ELFParser::new(&elf, interp_base).unwrap();
    let base_addr = elf_parser.base();
    assert_eq!(base_addr, 0);

    let segments = elf_parser.ph_load().collect::<Vec<_>>();
    assert_eq!(segments.len(), 4);
    let mut last_start = 0;
    for segment in segments.iter() {
        // start vaddr should be sorted
        assert!(segment.vaddr > last_start);
        last_start = segment.vaddr;
    }
    assert_eq!(segments[0].vaddr, 0x400000);

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

    let stack_data = kernel_elf_parser::app_stack_region(&args, &envs, &auxv, ustack_end);
    // The first 8 bytes of the stack is the number of arguments.
    assert_eq!(stack_data[0..8], [3, 0, 0, 0, 0, 0, 0, 0]);
}

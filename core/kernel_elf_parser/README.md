# kernel-elf-parser

[![Crates.io](https://img.shields.io/crates/v/kernel-elf-parser)](https://crates.io/crates/kernel-elf-parser)
[![Docs.rs](https://docs.rs/kernel-elf-parser/badge.svg)](https://docs.rs/kernel-elf-parser)
[![CI](https://github.com/Azure-stars/kernel-elf-parser/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/Azure-stars/kernel-elf-parser/actions/workflows/ci.yml)

A lightweight ELF parser written in Rust, providing assistance for loading applications into the kernel.

It reads the data of the ELF file, and generates Sections, Relocations, Segments and so on.

It also generate a layout of the user stack according to the given user parameters and environment variables,which will be
used for loading a given application into the physical memory of the kernel.

## Examples

```rust
use std::collections::BTreeMap;
use kernel_elf_parser::{AuxEntry, AuxType};
let args: Vec<String> = vec!["arg1".to_string(), "arg2".to_string(), "arg3".to_string()];
let envs: Vec<String> = vec!["LOG=file".to_string()];
let mut auxv = [
    AuxEntry::new(AuxType::PHDR, 0x1000),
    AuxEntry::new(AuxType::PHENT, 1024),
    AuxEntry::new(AuxType::PHNUM, 10),
    AuxEntry::new(AuxType::PAGESZ, 0x1000),
    AuxEntry::new(AuxType::ENTRY, 0x1000),
];
// The highest address of the user stack.
let ustack_end = 0x4000_0000;

let stack_data = kernel_elf_parser::app_stack_region(&args, &envs, &auxv, ustack_end);

// args length
assert_eq!(stack_data[0..8], [3, 0, 0, 0, 0, 0, 0, 0]);
```

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec::Vec;

use unittest::def_test;

use super::*;

/// Build a blob: header + `entries` + names.
fn blob(addr_high: u32, entries: &[(u32, u32, u32)], names: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&SYMTAB_MAGIC.to_le_bytes());
    bytes.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&addr_high.to_le_bytes());
    bytes.extend_from_slice(&(names.len() as u32).to_le_bytes());
    for (addr_lo, size, name_off) in entries {
        bytes.extend_from_slice(&addr_lo.to_le_bytes());
        bytes.extend_from_slice(&size.to_le_bytes());
        bytes.extend_from_slice(&name_off.to_le_bytes());
    }
    bytes.extend_from_slice(names);
    bytes
}

#[def_test]
fn parse_rejects_bad_magic_and_truncated_blobs() {
    assert!(KernelSymtab::parse(&[]).is_none());
    assert!(KernelSymtab::parse(&[0u8; 8]).is_none());
    let mut bad = blob(0xffff_8000, &[], &[]);
    bad[0] = 0x00; // corrupt magic
    assert!(KernelSymtab::parse(&bad).is_none());
    // Truncated entry region.
    let full = blob(0xffff_8000, &[(0x1000, 8, 0)], b"foo\0");
    assert!(KernelSymtab::parse(&full[..full.len() - 1]).is_none());
}

#[def_test]
fn lookup_handles_first_last_and_gaps() {
    let names = b"alpha\0beta\0gamma\0";
    let entries = [(0x1000, 0x10, 0), (0x2000, 0x10, 6), (0x3000, 0x10, 11)];
    let bytes = blob(0xffff_8000, &entries, names);
    let symtab = KernelSymtab::parse(&bytes).unwrap();
    let base = 0xffff_8000_0000_0000usize;

    // First symbol, offset inside.
    let symbol = symtab.lookup(base | 0x1005).unwrap();
    assert_eq!(symbol.name, b"alpha");
    assert_eq!(symbol.offset, 5);
    assert_eq!(symbol.size, 0x10);
    // Last symbol.
    assert_eq!(symtab.lookup(base | 0x300f).unwrap().name, b"gamma");
    // Gap between symbols: not covered.
    assert!(symtab.lookup(base | 0x1010).is_none());
    // Before the first symbol.
    assert!(symtab.lookup(base | 0x0fff).is_none());
    // Past the last symbol.
    assert!(symtab.lookup(base | 0x3010).is_none());
}

#[def_test]
fn lookup_uses_next_symbol_as_boundary_when_size_is_zero() {
    let names = b"a\0b\0";
    let entries = [(0x1000, 0, 0), (0x2000, 0x10, 2)];
    let bytes = blob(0xffff_8000, &entries, names);
    let symtab = KernelSymtab::parse(&bytes).unwrap();
    let base = 0xffff_8000_0000_0000usize;

    // Size-0 entry extends to the next symbol.
    assert_eq!(symtab.lookup(base | 0x1fff).unwrap().name, b"a");
    // The next symbol's start belongs to that symbol, not the size-0 one.
    assert_eq!(symtab.lookup(base | 0x2000).unwrap().name, b"b");
}

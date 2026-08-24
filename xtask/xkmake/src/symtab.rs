// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Compact kernel symbol table (kallsyms-style) generation.
//!
//! Extracts function symbols from the kernel ELF and writes a compact blob
//! to `$TARGET_DIR/kbuild/ksymtab.bin`, which `util/backtrace` embeds into
//! the kernel image as a plain `.rodata` static (no dedicated linker
//! section). The bootstrap converges because the table only contains
//! `.text` symbols and `.text` precedes `.rodata` in the linker script, so
//! growing the blob between the first and second link never shifts the
//! addresses stored in the table.
//!
//! The blob format is mirrored by `util/backtrace/src/symtab.rs`:
//!
//! ```text
//! header (16 bytes):  magic u32 | count u32 | addr_high u32 | name_blob_len u32
//! entries[count]:     addr_lo u32 | size u32 | name_off u32   (12 bytes each)
//! name blob:          NUL-terminated names, offsets relative to blob start
//! ```

use std::{fs, path::Path};

use goblin::elf::{
    Elf,
    section_header::{SHF_ALLOC, SHF_EXECINSTR, SHN_LORESERVE, SHN_UNDEF},
    sym::STT_FUNC,
};

use crate::{
    context::BuildContext,
    error::{Error, IoResultExt, Result},
};

const SYMTAB_MAGIC: u32 = 0x584b_5354; // "XKST"
const HEADER_SIZE: usize = 16;
const ENTRY_SIZE: usize = 12;

#[repr(C)]
struct SymtabEntry {
    addr_lo: u32,
    size: u32,
    name_off: u32,
}

/// Generate the symbol table blob from the kernel ELF.
///
/// Returns whether the blob changed on disk (i.e. whether the kernel must
/// be relinked so the embedded table catches up with the symbol layout).
pub(crate) fn generate(context: &BuildContext) -> Result<bool> {
    let blob = build_blob(&context.cargo_elf)?;
    let output = context.target_dir.join("kbuild").join("ksymtab.bin");
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent).with_path(parent)?;
    }
    write_if_changed(&output, &blob)
}

/// Ensure the blob input always exists when `KFEAT_SYMTAB` is disabled.
///
/// `util/backtrace/build.rs` declares `cargo:rerun-if-changed` on
/// `$TARGET_DIR/kbuild/ksymtab.bin` so the embedded table catches up when the
/// blob appears or changes. Cargo treats a *missing* `rerun-if-changed` input
/// as permanently dirty, which would rebuild `backtrace` and every transitive
/// dependent on each `xkmake build`. Writing the empty blob here keeps the
/// input present and content-stable across builds, so the build script only
/// reruns when the symbol table actually changes.
pub(crate) fn ensure_empty(context: &BuildContext) -> Result<()> {
    let output = context.target_dir.join("kbuild").join("ksymtab.bin");
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent).with_path(parent)?;
    }
    write_if_changed(&output, &empty_blob())?;
    Ok(())
}

fn build_blob(elf_path: &Path) -> Result<Vec<u8>> {
    let bytes = fs::read(elf_path).with_path(elf_path)?;
    let elf = Elf::parse(&bytes).map_err(|err| {
        Error::Message(format!(
            "failed to parse {} for symbol table extraction: {err}",
            elf_path.display()
        ))
    })?;

    let is_exec = elf
        .section_headers
        .iter()
        .map(|section| {
            section.sh_flags & u64::from(SHF_ALLOC) != 0
                && section.sh_flags & u64::from(SHF_EXECINSTR) != 0
        })
        .collect::<Vec<_>>();

    let mut symbols: Vec<(u64, u64, Vec<u8>)> = Vec::new();
    for symbol in &elf.syms {
        if symbol.st_type() != STT_FUNC || symbol.st_value == 0 {
            continue;
        }
        let index = symbol.st_shndx;
        if index == SHN_UNDEF as usize
            || index >= SHN_LORESERVE as usize
            || !is_exec.get(index).copied().unwrap_or(false)
        {
            continue;
        }
        let name = elf.strtab.get_at(symbol.st_name).unwrap_or("");
        if name.is_empty() {
            continue;
        }
        symbols.push((symbol.st_value, symbol.st_size, name.as_bytes().to_vec()));
    }
    symbols.sort_by_key(|(address, ..)| *address);
    symbols.dedup_by(|a, b| a.0 == b.0);

    if symbols.is_empty() {
        return Ok(empty_blob());
    }

    let addr_high = (symbols[0].0 >> 32) as u32;
    let mut entries = Vec::with_capacity(symbols.len());
    let mut names: Vec<u8> = Vec::new();
    for (address, size, name) in symbols {
        // The blob stores one shared high-32 address word; drop the rare
        // symbol living outside the main kernel address window.
        if (address >> 32) as u32 != addr_high {
            continue;
        }
        let name_off = names.len() as u32;
        names.extend_from_slice(&name);
        names.push(0);
        entries.push(SymtabEntry {
            addr_lo: (address & 0xffff_ffff) as u32,
            size: size.min(u64::from(u32::MAX)) as u32,
            name_off,
        });
    }

    let mut blob = Vec::with_capacity(HEADER_SIZE + entries.len() * ENTRY_SIZE + names.len());
    blob.extend_from_slice(&SYMTAB_MAGIC.to_le_bytes());
    blob.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    blob.extend_from_slice(&addr_high.to_le_bytes());
    blob.extend_from_slice(&(names.len() as u32).to_le_bytes());
    for entry in &entries {
        blob.extend_from_slice(&entry.addr_lo.to_le_bytes());
        blob.extend_from_slice(&entry.size.to_le_bytes());
        blob.extend_from_slice(&entry.name_off.to_le_bytes());
    }
    blob.extend_from_slice(&names);
    Ok(blob)
}

/// A valid empty table: no symbols, no annotations.
fn empty_blob() -> Vec<u8> {
    let mut blob = Vec::with_capacity(HEADER_SIZE);
    blob.extend_from_slice(&SYMTAB_MAGIC.to_le_bytes());
    blob.extend_from_slice(&0u32.to_le_bytes()); // count
    blob.extend_from_slice(&0u32.to_le_bytes()); // addr_high
    blob.extend_from_slice(&0u32.to_le_bytes()); // name_blob_len
    blob
}

fn write_if_changed(path: &Path, contents: &[u8]) -> Result<bool> {
    match fs::read(path) {
        Ok(existing) if existing == contents => return Ok(false),
        _ => {}
    }
    fs::write(path, contents).with_path(path)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_blob_is_valid_and_stable() {
        let blob = empty_blob();
        assert_eq!(blob.len(), HEADER_SIZE);
        assert_eq!(blob[0..4], SYMTAB_MAGIC.to_le_bytes());
        // Round-trip through the kernel-side parser contract: count 0.
        assert_eq!(u32::from_le_bytes(blob[4..8].try_into().unwrap()), 0);
    }
}

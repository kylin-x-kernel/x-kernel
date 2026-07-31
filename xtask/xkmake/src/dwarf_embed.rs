// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Embed selected DWARF sections into X-Kernel alloc sections.
//!
//! The kernel link script reserves allocated `debug_*` sections (without the
//! leading dot) next to the corresponding non-allocated `.debug_*` sections.
//! [`embed_dwarf`] copies each `.debug_*` payload into its allocated twin so the
//! debug information survives `rust-objcopy --strip-all`, then compacts the
//! stripped sections out of the file.

use std::{error::Error, fs, ops::Range, path::Path};

use goblin::{
    container::{Container, Ctx},
    elf::{
        Elf,
        program_header::ProgramHeader,
        section_header::{
            SHF_ALLOC, SHF_INFO_LINK, SHN_LORESERVE, SHN_UNDEF, SHT_NOBITS, SHT_REL, SHT_RELA,
            SHT_SYMTAB, SectionHeader,
        },
        sym::Sym,
    },
};
use scroll::{Pread, Pwrite};

const DWARF_SECTIONS: &[&str] = &[
    "debug_abbrev",
    "debug_addr",
    "debug_aranges",
    "debug_info",
    "debug_line",
    "debug_line_str",
    "debug_ranges",
    "debug_rnglists",
    "debug_str",
    "debug_str_offsets",
];

struct CopyOperation {
    source: Range<usize>,
    destination_start: usize,
}

/// Transform the ELF at `path` in place: copy `.debug_*` payloads into their
/// allocated twins and compact the stripped sections out of the file.
pub(crate) fn embed_dwarf(path: &Path) -> crate::error::Result<()> {
    run_embed(path).map_err(|err| {
        crate::error::Error::Message(format!(
            "failed to embed DWARF sections into {}: {err}",
            path.display()
        ))
    })
}

fn run_embed(path: &Path) -> Result<(), Box<dyn Error>> {
    let bytes = fs::read(path)?;
    let output = transform(&bytes)?;
    fs::write(path, output)?;
    Ok(())
}

fn transform(bytes: &[u8]) -> Result<Vec<u8>, Box<dyn Error>> {
    let elf = Elf::parse(bytes)?;
    if !elf.is_64 {
        return Err("only ELF64 kernel images are supported".into());
    }
    let ctx = elf_context(&elf)?;

    let copy_ops = dwarf_copy_operations(bytes.len(), &elf)?;

    let removed = removable_debug_sections(&elf);
    if removed.is_empty() {
        let mut output = bytes.to_vec();
        apply_copy_operations(&mut output, &copy_ops);
        return Ok(output);
    }

    let index_map = build_section_index_map(elf.section_headers.len(), &removed);
    let remove_ranges = removable_file_ranges(&elf, &removed)?;
    let headers = remap_headers(&elf, &index_map, &remove_ranges)?;
    let old_shstrndx = usize::from(elf.header.e_shstrndx);
    let new_shstrndx = index_map
        .get(old_shstrndx)
        .and_then(|index| *index)
        .ok_or("section header string table was removed")?;

    let old_shoff = elf.header.e_shoff as usize;
    let old_shdr_size = elf.section_headers.len() * usize::from(elf.header.e_shentsize);
    checked_range(
        bytes.len(),
        old_shoff,
        old_shdr_size,
        "section header table",
    )?;

    let mut embedded = bytes.to_vec();
    apply_copy_operations(&mut embedded, &copy_ops);

    let mut output = compact_file_data(&embedded, old_shoff, &remove_ranges)?;
    align_to(&mut output, 8);

    let new_shoff = output.len();
    let mut header = elf.header;
    header.e_shoff = new_shoff as u64;
    header.e_shnum = headers
        .len()
        .try_into()
        .map_err(|_| "too many section headers after DWARF stripping")?;
    header.e_shstrndx = new_shstrndx
        .try_into()
        .map_err(|_| "section header string table index does not fit in ELF header")?;

    rewrite_symbol_section_indices(&mut output, &headers, &index_map, ctx)?;
    rewrite_program_header_offsets(&mut output, &elf, &remove_ranges, ctx)?;
    output.pwrite_with(header, 0, ctx.le)?;
    append_section_headers(&mut output, &headers, ctx)?;

    let reparsed = Elf::parse(&output)?;
    validate_output(&reparsed)?;
    validate_program_headers(&reparsed, output.len())?;

    Ok(output)
}

fn elf_context(elf: &Elf<'_>) -> Result<Ctx, Box<dyn Error>> {
    let container = match elf.header.container()? {
        Container::Big => Container::Big,
        Container::Little => return Err("only ELF64 kernel images are supported".into()),
    };
    Ok(Ctx::new(container, elf.header.endianness()?))
}

fn dwarf_copy_operations(
    file_len: usize,
    elf: &Elf<'_>,
) -> Result<Vec<CopyOperation>, Box<dyn Error>> {
    let mut operations = Vec::new();
    for section in DWARF_SECTIONS {
        let source_name = format!(".{section}");
        let Some(source) = find_section(elf, &source_name) else {
            continue;
        };
        if source.sh_size == 0 {
            continue;
        }

        let Some(destination) = find_section(elf, section) else {
            return Err(format!("missing destination section `{section}`").into());
        };
        if destination.sh_flags & u64::from(SHF_ALLOC) == 0 {
            return Err(format!("destination section `{section}` is not allocated").into());
        }
        if source.sh_size != destination.sh_size {
            return Err(format!(
                "section size mismatch for `{section}`: source={} destination={}",
                source.sh_size, destination.sh_size
            )
            .into());
        }

        let source_range = section_file_range(file_len, source, &source_name)?;
        let destination_range = section_file_range(file_len, destination, section)?;
        operations.push(CopyOperation {
            source: source_range,
            destination_start: destination_range.start,
        });
    }
    Ok(operations)
}

fn apply_copy_operations(bytes: &mut [u8], operations: &[CopyOperation]) {
    for operation in operations {
        bytes.copy_within(operation.source.clone(), operation.destination_start);
    }
}

fn removable_debug_sections(elf: &Elf<'_>) -> Vec<usize> {
    elf.section_headers
        .iter()
        .enumerate()
        .filter_map(|(index, section)| {
            let name = section_name(elf, section);
            let is_debug = name.starts_with(".debug_");
            let is_alloc = section.sh_flags & u64::from(SHF_ALLOC) != 0;
            (is_debug && !is_alloc).then_some(index)
        })
        .collect()
}

fn build_section_index_map(section_count: usize, removed: &[usize]) -> Vec<Option<usize>> {
    let mut removed_iter = removed.iter().copied().peekable();
    let mut next_index = 0;
    let mut map = Vec::with_capacity(section_count);
    for index in 0..section_count {
        if removed_iter.peek() == Some(&index) {
            removed_iter.next();
            map.push(None);
        } else {
            map.push(Some(next_index));
            next_index += 1;
        }
    }
    map
}

fn removable_file_ranges(
    elf: &Elf<'_>,
    removed: &[usize],
) -> Result<Vec<Range<usize>>, Box<dyn Error>> {
    let mut ranges = Vec::new();
    for index in removed {
        let section = &elf.section_headers[*index];
        if section.sh_type == SHT_NOBITS || section.sh_size == 0 {
            continue;
        }
        let start = section.sh_offset as usize;
        let size = section.sh_size as usize;
        ranges.push(checked_range(
            elf.header.e_shoff as usize,
            start,
            size,
            section_name(elf, section),
        )?);
    }

    let old_shoff = elf.header.e_shoff as usize;
    let kept_starts = kept_file_starts_before_shdr(elf, removed, old_shoff);
    for range in &mut ranges {
        if let Some(next_kept_start) = kept_starts
            .iter()
            .copied()
            .find(|start| *start >= range.end)
        {
            range.end = next_kept_start;
        } else {
            range.end = old_shoff;
        }
    }

    ranges.sort_by_key(|range| range.start);
    let mut merged: Vec<Range<usize>> = Vec::new();
    for range in ranges {
        if let Some(last) = merged.last_mut()
            && last.end >= range.start
        {
            last.end = last.end.max(range.end);
            continue;
        }
        if merged.last().is_some_and(|last| range.start < last.end) {
            return Err("debug section file ranges overlap unexpectedly".into());
        }
        merged.push(range);
    }
    Ok(merged)
}

fn kept_file_starts_before_shdr(elf: &Elf<'_>, removed: &[usize], old_shoff: usize) -> Vec<usize> {
    let mut starts = elf
        .section_headers
        .iter()
        .enumerate()
        .filter_map(|(index, section)| {
            let is_removed = removed.binary_search(&index).is_ok();
            let has_file_data = section.sh_type != SHT_NOBITS && section.sh_size != 0;
            let start = section.sh_offset as usize;
            (!is_removed && has_file_data && start < old_shoff).then_some(start)
        })
        .collect::<Vec<_>>();
    starts.sort_unstable();
    starts.dedup();
    starts
}

fn remap_headers(
    elf: &Elf<'_>,
    index_map: &[Option<usize>],
    removed_ranges: &[Range<usize>],
) -> Result<Vec<SectionHeader>, Box<dyn Error>> {
    let mut headers = Vec::new();
    for (old_index, section) in elf.section_headers.iter().enumerate() {
        if index_map[old_index].is_none() {
            continue;
        }

        let mut section = section.clone();
        if section.sh_type != SHT_NOBITS && section.sh_size != 0 {
            section.sh_offset =
                remap_file_offset(section.sh_offset, section.sh_size, removed_ranges)?;
        }

        section.sh_link = remap_section_reference(section.sh_link, index_map).unwrap_or(0);
        if section.sh_type == SHT_REL
            || section.sh_type == SHT_RELA
            || section.sh_flags & u64::from(SHF_INFO_LINK) != 0
        {
            section.sh_info = remap_section_reference(section.sh_info, index_map).unwrap_or(0);
        }

        headers.push(section);
    }
    Ok(headers)
}

fn remap_file_offset(
    offset: u64,
    size: u64,
    removed_ranges: &[Range<usize>],
) -> Result<u64, Box<dyn Error>> {
    let start = offset as usize;
    let end = start
        .checked_add(size as usize)
        .ok_or("section file range overflows usize")?;
    let mut removed_before = 0usize;

    for range in removed_ranges {
        if range.end <= start {
            removed_before += range.len();
        } else if range.start < end {
            return Err("kept section overlaps a removed debug section".into());
        }
    }

    Ok((start - removed_before) as u64)
}

fn remap_section_reference(index: u32, index_map: &[Option<usize>]) -> Option<u32> {
    let index = index as usize;
    if index == 0 || index >= index_map.len() {
        return Some(index as u32);
    }
    index_map[index].map(|new_index| new_index as u32)
}

fn compact_file_data(
    bytes: &[u8],
    old_shoff: usize,
    removed_ranges: &[Range<usize>],
) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut output = Vec::with_capacity(old_shoff);
    let mut cursor = 0usize;
    for range in removed_ranges {
        if range.end > old_shoff {
            return Err("removed debug section extends beyond the old section header table".into());
        }
        output.extend_from_slice(&bytes[cursor..range.start]);
        cursor = range.end;
    }
    output.extend_from_slice(&bytes[cursor..old_shoff]);
    Ok(output)
}

fn rewrite_symbol_section_indices(
    bytes: &mut [u8],
    headers: &[SectionHeader],
    index_map: &[Option<usize>],
    ctx: Ctx,
) -> Result<(), Box<dyn Error>> {
    for section in headers {
        if section.sh_type != SHT_SYMTAB || section.sh_entsize == 0 {
            continue;
        }

        let entry_size = section.sh_entsize as usize;
        let range = section_file_range(bytes.len(), section, ".symtab")?;
        if range.len() % entry_size != 0 {
            return Err("symbol table size is not a multiple of entry size".into());
        }

        for offset in (range.start..range.end).step_by(entry_size) {
            let mut symbol = bytes.pread_with::<Sym>(offset, ctx)?;
            if symbol.st_shndx < SHN_LORESERVE as usize
                && let Some(new_index) = remap_symbol_section(symbol.st_shndx, index_map)
            {
                symbol.st_shndx = new_index;
                bytes.pwrite_with(symbol, offset, ctx)?;
            }
        }
    }
    Ok(())
}

fn remap_symbol_section(index: usize, index_map: &[Option<usize>]) -> Option<usize> {
    if index == SHN_UNDEF as usize || index >= index_map.len() {
        return None;
    }
    Some(index_map[index].unwrap_or(SHN_UNDEF as usize))
}

fn append_section_headers(
    output: &mut Vec<u8>,
    headers: &[SectionHeader],
    ctx: Ctx,
) -> Result<(), Box<dyn Error>> {
    let entry_size = SectionHeader::size(ctx);
    let start = output.len();
    output.resize(start + headers.len() * entry_size, 0);
    for (index, header) in headers.iter().enumerate() {
        let offset = start + index * entry_size;
        output.pwrite_with(header.clone(), offset, ctx)?;
    }
    Ok(())
}

fn validate_output(elf: &Elf<'_>) -> Result<(), Box<dyn Error>> {
    for section in &elf.section_headers {
        let name = section_name(elf, section);
        if name.starts_with(".debug_") {
            return Err(format!("debug section `{name}` was not removed").into());
        }
    }
    Ok(())
}

/// Compaction removes non-allocated `.debug_*` sections from the middle of the
/// file, shifting every byte that follows them. Section header offsets are
/// remapped in [`remap_headers`], but the program header table is copied
/// verbatim and otherwise left untouched. Sections that are described by *both*
/// a section header and a program header -- notably RISC-V's `.riscv.attributes`
/// (`PT_RISCV_ATTRIBUTES`), which the linker places among the debug sections --
/// end up with a stale `p_offset` pointing past the end of the trimmed file,
/// which makes the subsequent `objcopy` abort. Remap each program header's
/// `p_offset` the same way section offsets are remapped.
fn rewrite_program_header_offsets(
    bytes: &mut [u8],
    elf: &Elf<'_>,
    removed_ranges: &[Range<usize>],
    ctx: Ctx,
) -> Result<(), Box<dyn Error>> {
    let phoff = elf.header.e_phoff as usize;
    let phentsize = usize::from(elf.header.e_phentsize);
    let phnum = usize::from(elf.header.e_phnum);
    if phoff == 0 || phnum == 0 {
        return Ok(());
    }

    // The program header table sits at the start of the file, ahead of every
    // removed debug section, so compaction never shifts it. Assert that so the
    // entries can be patched in place.
    let table_end = phoff
        .checked_add(
            phnum
                .checked_mul(phentsize)
                .ok_or("program header table size overflows usize")?,
        )
        .ok_or("program header table end overflows usize")?;
    if removed_ranges
        .first()
        .is_some_and(|first| table_end > first.start)
    {
        return Err(
            "program header table overlaps a removed debug section; in-place update unsupported"
                .into(),
        );
    }

    for index in 0..phnum {
        let offset = phoff + index * phentsize;
        let mut entry: ProgramHeader = bytes.pread_with(offset, ctx)?;
        // Segments with no on-disk contents (e.g. `PT_GNU_STACK`) keep offset 0.
        if entry.p_filesz == 0 && entry.p_offset == 0 {
            continue;
        }
        entry.p_offset = remap_file_offset(entry.p_offset, entry.p_filesz, removed_ranges)?;
        bytes.pwrite_with(entry, offset, ctx)?;
    }
    Ok(())
}

/// Guard against regressions of the class of bug fixed by
/// [`rewrite_program_header_offsets`]: every segment's on-disk range must stay
/// within the trimmed file.
fn validate_program_headers(elf: &Elf<'_>, file_len: usize) -> Result<(), Box<dyn Error>> {
    for phdr in &elf.program_headers {
        if phdr.p_filesz == 0 {
            continue;
        }
        let start = phdr.p_offset as usize;
        let end = start
            .checked_add(phdr.p_filesz as usize)
            .ok_or("program header file range overflows usize")?;
        if end > file_len {
            return Err(format!(
                "program header (type {:#x}) extends past end of file: offset {start:#x}, size \
                 {:#x}, file len {file_len}",
                phdr.p_type, phdr.p_filesz
            )
            .into());
        }
    }
    Ok(())
}

fn find_section<'a>(elf: &'a Elf<'a>, name: &str) -> Option<&'a SectionHeader> {
    elf.section_headers
        .iter()
        .find(|section| section_name(elf, section) == name)
}

fn section_name<'a>(elf: &'a Elf<'a>, section: &SectionHeader) -> &'a str {
    elf.shdr_strtab.get_at(section.sh_name).unwrap_or("")
}

fn section_file_range(
    file_len: usize,
    section: &SectionHeader,
    name: &str,
) -> Result<Range<usize>, Box<dyn Error>> {
    if section.sh_type == SHT_NOBITS {
        return Err(format!("section `{name}` has no file contents").into());
    }
    checked_range(
        file_len,
        section.sh_offset as usize,
        section.sh_size as usize,
        name,
    )
}

fn checked_range(
    limit: usize,
    start: usize,
    size: usize,
    label: &str,
) -> Result<Range<usize>, Box<dyn Error>> {
    let end = start
        .checked_add(size)
        .ok_or_else(|| format!("range for `{label}` overflows usize"))?;
    if end > limit {
        return Err(format!("range for `{label}` exceeds file bounds").into());
    }
    Ok(start..end)
}

fn align_to(bytes: &mut Vec<u8>, align: usize) {
    let padding = bytes.len().next_multiple_of(align) - bytes.len();
    bytes.resize(bytes.len() + padding, 0);
}

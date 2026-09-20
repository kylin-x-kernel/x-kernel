// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Validation and address planning for executable ELF images.

use alloc::vec::Vec;
use core::ops::Range;

use kernel_elf_parser::ELFHeaders;
use kerrno::{KError, KResult};
use memaddr::PAGE_SIZE_4K;
use xmas_elf::{
    header::{Class, Data, Machine, Type as ElfHeaderType, Version},
    program::{Flags, Type as ProgramHeaderType},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ElfType {
    Executable,
    PositionIndependent,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct ValidatedLoadSegment {
    memory_start: usize,
    memory_end: usize,
    pub(super) mapping_start: usize,
    pub(super) mapping_end: usize,
    pub(super) file_start: u64,
    pub(super) file_data_end: u64,
    pub(super) flags: Flags,
}

impl ValidatedLoadSegment {
    pub(super) fn mapping_size(&self) -> usize {
        self.mapping_end - self.mapping_start
    }
}

#[derive(Debug)]
pub(super) struct ValidatedElfImage {
    elf_type: ElfType,
    entry: usize,
    program_header_address: usize,
    program_header_entry_size: usize,
    program_header_count: usize,
    load_start: usize,
    load_end: usize,
    required_alignment: usize,
    segments: Vec<ValidatedLoadSegment>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ExecLayoutPlan {
    executable_bias: usize,
    interpreter_bias: Option<usize>,
}

impl ExecLayoutPlan {
    pub(super) fn new(
        executable: &ValidatedElfImage,
        interpreter: Option<&ValidatedElfImage>,
    ) -> KResult<Self> {
        let executable_bias = match executable.elf_type() {
            ElfType::Executable => 0,
            ElfType::PositionIndependent => checked_align_up(
                kaddr_layout::USER_SPACE_BASE,
                executable.required_alignment(),
            )?,
        };
        let executable_range = executable.load_range(executable_bias)?;
        let load_limit = user_load_limit()?;
        if executable_range.start < load_limit.start || executable_range.end > load_limit.end {
            return Err(KError::NoMemory);
        }

        let interpreter_bias = interpreter
            .map(|interpreter| match interpreter.elf_type() {
                ElfType::Executable => {
                    let range = interpreter.load_range(0)?;
                    if range.start < load_limit.start
                        || range.end > load_limit.end
                        || ranges_overlap(&range, &executable_range)
                    {
                        return Err(KError::NoMemory);
                    }
                    Ok(0)
                }
                ElfType::PositionIndependent => find_dynamic_load_bias(
                    interpreter,
                    kaddr_layout::USER_INTERP_BASE,
                    load_limit,
                    core::slice::from_ref(&executable_range),
                )
                .ok_or(KError::NoMemory),
            })
            .transpose()?;

        Ok(Self {
            executable_bias,
            interpreter_bias,
        })
    }

    pub(super) fn executable_bias(&self) -> usize {
        self.executable_bias
    }

    pub(super) fn interpreter_bias(&self) -> Option<usize> {
        self.interpreter_bias
    }
}

impl ValidatedElfImage {
    pub(super) fn new(headers: &ELFHeaders<'_>, file_size: u64) -> KResult<Self> {
        validate_identification(headers)?;
        let elf_type = validate_header_type(headers)?;
        let program_header_range = program_header_range(headers)?;
        if program_header_range.end > file_size {
            return Err(KError::InvalidExecutable);
        }

        let mut segments = Vec::new();
        let mut load_start = usize::MAX;
        let mut load_end = 0usize;
        let mut required_alignment = PAGE_SIZE_4K;
        let mut previous_virtual_address = None;

        for header in headers
            .ph
            .iter()
            .filter(|header| header.get_type() == Ok(ProgramHeaderType::Load))
        {
            if header.file_size > header.mem_size {
                return Err(KError::InvalidExecutable);
            }

            let virtual_address =
                usize::try_from(header.virtual_addr).map_err(|_| KError::InvalidExecutable)?;
            if previous_virtual_address.is_some_and(|previous| virtual_address < previous) {
                return Err(KError::InvalidExecutable);
            }
            previous_virtual_address = Some(virtual_address);

            let memory_size =
                usize::try_from(header.mem_size).map_err(|_| KError::InvalidExecutable)?;
            let alignment = validate_alignment(header.align)?;
            required_alignment = required_alignment.max(alignment);
            if virtual_address % alignment
                != usize::try_from(header.offset % header.align.max(1))
                    .map_err(|_| KError::InvalidExecutable)?
            {
                return Err(KError::InvalidExecutable);
            }
            if virtual_address % PAGE_SIZE_4K
                != usize::try_from(header.offset % PAGE_SIZE_4K as u64)
                    .map_err(|_| KError::InvalidExecutable)?
            {
                return Err(KError::InvalidExecutable);
            }

            let file_data_end = header
                .offset
                .checked_add(header.file_size)
                .ok_or(KError::InvalidExecutable)?;
            if header.offset > file_size || file_data_end > file_size {
                return Err(KError::InvalidExecutable);
            }

            if memory_size == 0 {
                continue;
            }
            let memory_end = virtual_address
                .checked_add(memory_size)
                .ok_or(KError::InvalidExecutable)?;
            let mapping_start = align_down(virtual_address, PAGE_SIZE_4K);
            let mapping_end = checked_align_up(memory_end, PAGE_SIZE_4K)?;
            let file_start = align_down_u64(header.offset, PAGE_SIZE_4K as u64);

            load_start = load_start.min(mapping_start);
            load_end = load_end.max(mapping_end);
            segments.push(ValidatedLoadSegment {
                memory_start: virtual_address,
                memory_end,
                mapping_start,
                mapping_end,
                file_start,
                file_data_end,
                flags: header.flags,
            });
        }

        if segments.is_empty() {
            return Err(KError::InvalidExecutable);
        }
        trim_overlapping_segment_pages(&mut segments)?;

        let entry = usize::try_from(headers.header.pt2.entry_point())
            .map_err(|_| KError::InvalidExecutable)?;
        let is_declared_executable = segments.iter().any(|segment| {
            segment.flags.is_execute()
                && (segment.memory_start..segment.memory_end).contains(&entry)
        });
        let is_mapped_executable = segments.iter().any(|segment| {
            segment.flags.is_execute()
                && (segment.mapping_start..segment.mapping_end).contains(&entry)
        });
        if !is_declared_executable || !is_mapped_executable {
            return Err(KError::InvalidExecutable);
        }
        let program_header_address =
            program_header_address(headers, program_header_range.clone(), &segments)?;

        Ok(Self {
            elf_type,
            entry,
            program_header_address,
            program_header_entry_size: usize::from(headers.header.pt2.ph_entry_size()),
            program_header_count: usize::from(headers.header.pt2.ph_count()),
            load_start,
            load_end,
            required_alignment,
            segments,
        })
    }

    pub(super) fn elf_type(&self) -> ElfType {
        self.elf_type
    }

    pub(super) fn entry(&self, load_bias: usize) -> KResult<usize> {
        load_bias
            .checked_add(self.entry)
            .ok_or(KError::InvalidExecutable)
    }

    pub(super) fn program_header_address(&self, load_bias: usize) -> KResult<usize> {
        load_bias
            .checked_add(self.program_header_address)
            .ok_or(KError::InvalidExecutable)
    }

    pub(super) fn program_header_entry_size(&self) -> usize {
        self.program_header_entry_size
    }

    pub(super) fn program_header_count(&self) -> usize {
        self.program_header_count
    }

    pub(super) fn load_start(&self) -> usize {
        self.load_start
    }

    #[cfg(unittest)]
    pub(super) fn load_end(&self) -> usize {
        self.load_end
    }

    pub(super) fn required_alignment(&self) -> usize {
        self.required_alignment
    }

    pub(super) fn load_range(&self, load_bias: usize) -> KResult<Range<usize>> {
        let start = load_bias
            .checked_add(self.load_start)
            .ok_or(KError::InvalidExecutable)?;
        let end = load_bias
            .checked_add(self.load_end)
            .ok_or(KError::InvalidExecutable)?;
        Ok(start..end)
    }

    pub(super) fn segments(&self) -> &[ValidatedLoadSegment] {
        &self.segments
    }
}

fn validate_identification(headers: &ELFHeaders<'_>) -> KResult {
    if headers.header.pt1.class() != Class::SixtyFour
        || headers.header.pt1.data() != Data::LittleEndian
        || headers.header.pt1.version() != Version::Current
        || headers.header.pt2.version() != 1
        || !machine_matches_current_arch(headers.header.pt2.machine().as_machine())
    {
        return Err(KError::InvalidExecutable);
    }
    Ok(())
}

fn validate_header_type(headers: &ELFHeaders<'_>) -> KResult<ElfType> {
    match headers.header.pt2.type_().as_type() {
        ElfHeaderType::Executable => Ok(ElfType::Executable),
        ElfHeaderType::SharedObject => Ok(ElfType::PositionIndependent),
        _ => Err(KError::InvalidExecutable),
    }
}

fn machine_matches_current_arch(machine: Machine) -> bool {
    #[cfg(target_arch = "aarch64")]
    return machine == Machine::AArch64;
    #[cfg(target_arch = "x86_64")]
    return machine == Machine::X86_64;
    #[cfg(target_arch = "riscv64")]
    return machine == Machine::RISC_V;
    #[cfg(target_arch = "loongarch64")]
    return machine == Machine::Other(258);
    #[allow(unreachable_code)]
    false
}

fn program_header_range(headers: &ELFHeaders<'_>) -> KResult<Range<u64>> {
    let start = headers.header.pt2.ph_offset();
    let size = u64::from(headers.header.pt2.ph_entry_size())
        .checked_mul(u64::from(headers.header.pt2.ph_count()))
        .ok_or(KError::InvalidExecutable)?;
    let end = start.checked_add(size).ok_or(KError::InvalidExecutable)?;
    Ok(start..end)
}

fn program_header_address(
    headers: &ELFHeaders<'_>,
    table_range: Range<u64>,
    segments: &[ValidatedLoadSegment],
) -> KResult<usize> {
    let table_size = usize::try_from(table_range.end - table_range.start)
        .map_err(|_| KError::InvalidExecutable)?;
    let table_size_u64 = u64::try_from(table_size).map_err(|_| KError::InvalidExecutable)?;
    let mut mapped_address = None;
    for header in headers
        .ph
        .iter()
        .filter(|header| header.get_type() == Ok(ProgramHeaderType::Load))
    {
        let file_end = header
            .offset
            .checked_add(header.file_size)
            .ok_or(KError::InvalidExecutable)?;
        if header.offset <= table_range.start && table_range.end <= file_end {
            let offset = table_range.start - header.offset;
            let address = header
                .virtual_addr
                .checked_add(offset)
                .ok_or(KError::InvalidExecutable)?;
            let address = usize::try_from(address).map_err(|_| KError::InvalidExecutable)?;
            if mapping_contains_file_range(address, &table_range, segments)? {
                mapped_address = Some(address);
                break;
            }
        }
    }
    let mapped_address = mapped_address.ok_or(KError::InvalidExecutable)?;

    let mut explicit_address = None;
    for header in headers
        .ph
        .iter()
        .filter(|header| header.get_type() == Ok(ProgramHeaderType::Phdr))
    {
        if explicit_address.is_some() {
            return Err(KError::InvalidExecutable);
        }
        if header.offset != table_range.start
            || header.file_size < table_size_u64
            || header.mem_size < table_size_u64
        {
            return Err(KError::InvalidExecutable);
        }
        let address =
            usize::try_from(header.virtual_addr).map_err(|_| KError::InvalidExecutable)?;
        if address != mapped_address {
            return Err(KError::InvalidExecutable);
        }
        explicit_address = Some(address);
    }
    Ok(explicit_address.unwrap_or(mapped_address))
}

fn mapping_contains_file_range(
    start: usize,
    file_range: &Range<u64>,
    segments: &[ValidatedLoadSegment],
) -> KResult<bool> {
    let size = usize::try_from(file_range.end - file_range.start)
        .map_err(|_| KError::InvalidExecutable)?;
    let end = start.checked_add(size).ok_or(KError::InvalidExecutable)?;
    for segment in segments {
        if segment.mapping_start <= start && end <= segment.mapping_end {
            let mapped_offset = u64::try_from(start - segment.mapping_start)
                .map_err(|_| KError::InvalidExecutable)?;
            let mapped_file_start = segment
                .file_start
                .checked_add(mapped_offset)
                .ok_or(KError::InvalidExecutable)?;
            return Ok(
                mapped_file_start == file_range.start && file_range.end <= segment.file_data_end
            );
        }
    }
    Ok(false)
}

fn validate_alignment(alignment: u64) -> KResult<usize> {
    if alignment <= 1 {
        return Ok(1);
    }
    let alignment = usize::try_from(alignment).map_err(|_| KError::InvalidExecutable)?;
    if !alignment.is_power_of_two() {
        return Err(KError::InvalidExecutable);
    }
    Ok(alignment)
}

fn trim_overlapping_segment_pages(segments: &mut Vec<ValidatedLoadSegment>) -> KResult {
    let mut index = 0;
    while index + 1 < segments.len() {
        // Page alignment may make adjacent segments share one page. A true
        // memory overlap would require splitting the earlier mapping around a
        // later MAP_FIXED replacement, so reject it instead of dropping data.
        if segments[index].memory_end > segments[index + 1].memory_start {
            return Err(KError::InvalidExecutable);
        }
        let next_start = segments[index + 1].mapping_start;
        if segments[index].mapping_end > next_start {
            segments[index].mapping_end = next_start;
        }
        if segments[index].mapping_start == segments[index].mapping_end {
            segments.remove(index);
        } else {
            index += 1;
        }
    }
    if segments.is_empty() {
        return Err(KError::InvalidExecutable);
    }
    Ok(())
}

fn align_down(value: usize, alignment: usize) -> usize {
    value & !(alignment - 1)
}

fn align_down_u64(value: u64, alignment: u64) -> u64 {
    value & !(alignment - 1)
}

fn checked_align_up(value: usize, alignment: usize) -> KResult<usize> {
    value
        .checked_add(alignment - 1)
        .map(|value| value & !(alignment - 1))
        .ok_or(KError::InvalidExecutable)
}

fn user_load_limit() -> KResult<Range<usize>> {
    let start = kaddr_layout::USER_SPACE_BASE;
    let end = kaddr_layout::USER_HEAP_BASE;
    if start >= end {
        return Err(KError::NoMemory);
    }
    Ok(start..end)
}

fn find_dynamic_load_bias(
    image: &ValidatedElfImage,
    hint: usize,
    limit: Range<usize>,
    occupied: &[Range<usize>],
) -> Option<usize> {
    let mut occupied = occupied.to_vec();
    occupied.sort_unstable_by_key(|range| range.start);

    find_dynamic_load_bias_from(image, hint, limit.clone(), &occupied).or_else(|| {
        if hint <= limit.start {
            None
        } else {
            find_dynamic_load_bias_from(image, limit.start, limit, &occupied)
        }
    })
}

fn find_dynamic_load_bias_from(
    image: &ValidatedElfImage,
    hint: usize,
    limit: Range<usize>,
    occupied: &[Range<usize>],
) -> Option<usize> {
    let alignment = image.required_alignment();
    let mut bias = bias_for_mapping_start(hint.max(limit.start), image.load_start(), alignment)?;

    loop {
        let candidate = image.load_range(bias).ok()?;
        if candidate.start < limit.start || candidate.end > limit.end {
            return None;
        }
        let Some(conflict) = occupied
            .iter()
            .find(|occupied| ranges_overlap(&candidate, occupied))
        else {
            return Some(bias);
        };
        bias = bias_for_mapping_start(conflict.end, image.load_start(), alignment)?;
    }
}

fn bias_for_mapping_start(
    minimum_start: usize,
    load_start: usize,
    alignment: usize,
) -> Option<usize> {
    let minimum_bias = minimum_start.saturating_sub(load_start);
    minimum_bias
        .checked_add(alignment - 1)
        .map(|bias| bias & !(alignment - 1))
}

fn ranges_overlap(left: &Range<usize>, right: &Range<usize>) -> bool {
    left.start < right.end && right.start < left.end
}

#[cfg(unittest)]
mod tests;

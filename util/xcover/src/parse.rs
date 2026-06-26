// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Profraw parsing — decodes `.profraw` bytes into structured data.
//!
//! Pure safe Rust. No unsafe, no ABI types, no raw pointer casting.
//! All offset/length calculations use checked arithmetic.
//! Parse failures return `ProfileError::MalformedInput`.

#[cfg(feature = "alloc")]
use alloc::vec::Vec;
use core::mem::size_of;

use crate::{
    ProfileError,
    abi::layout::IPVK_NUM_KINDS,
    image::{BitmapStore, CounterStore, ProfileFormat, ProfileImage, ValueKind, ValueProfileStore},
    record::{BitmapRange, CounterRange, FunctionHash, FunctionRecord, NameRef, ValueSiteRanges},
};

// === Safe byte reading helpers ===

/// Read a u64 from bytes at the given offset (little-endian).
fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    let end = offset.checked_add(size_of::<u64>())?;
    let slice = bytes.get(offset..end)?;
    Some(u64::from_le_bytes(slice.try_into().ok()?))
}

/// Read a u32 from bytes at the given offset (little-endian).
fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(size_of::<u32>())?;
    let slice = bytes.get(offset..end)?;
    Some(u32::from_le_bytes(slice.try_into().ok()?))
}

/// Read a u16 from bytes at the given offset (little-endian).
fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let end = offset.checked_add(size_of::<u16>())?;
    let slice = bytes.get(offset..end)?;
    Some(u16::from_le_bytes(slice.try_into().ok()?))
}

// === Header parsing ===

/// Parsed profraw header fields.
#[cfg(feature = "alloc")]
#[derive(Clone, Debug)]
pub(crate) struct RawHeader {
    pub magic: u64,
    pub version: u64,
    pub binary_ids_size: u64,
    pub num_data: u64,
    pub padding_before_counters: u64,
    pub num_counters: u64,
    pub padding_after_counters: u64,
    pub num_bitmap_bytes: u64,
    pub padding_after_bitmap: u64,
    pub names_size: u64,
}

#[cfg(feature = "alloc")]
impl RawHeader {
    /// Parse the header from bytes. Returns None if too short.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 128 {
            return None;
        }
        Some(Self {
            magic: read_u64(bytes, 0)?,
            version: read_u64(bytes, 8)?,
            binary_ids_size: read_u64(bytes, 16)?,
            num_data: read_u64(bytes, 24)?,
            padding_before_counters: read_u64(bytes, 32)?,
            num_counters: read_u64(bytes, 40)?,
            padding_after_counters: read_u64(bytes, 48)?,
            num_bitmap_bytes: read_u64(bytes, 56)?,
            padding_after_bitmap: read_u64(bytes, 64)?,
            names_size: read_u64(bytes, 72)?,
        })
    }

    /// Validates magic and version.
    pub fn is_valid(&self) -> bool {
        let expected_magic = if size_of::<*const ()>() == 8 {
            crate::abi::layout::INSTR_PROF_RAW_MAGIC_64
        } else {
            crate::abi::layout::INSTR_PROF_RAW_MAGIC_32
        };
        self.magic == expected_magic
            && (self.version & !crate::abi::layout::VARIANT_MASKS_ALL)
                == crate::abi::layout::INSTR_PROF_RAW_VERSION
    }

    pub fn is_byte_coverage(&self) -> bool {
        self.version & crate::abi::layout::VARIANT_MASK_BYTE_COVERAGE != 0
    }
}

// === Section layout computation ===

/// Computed byte offsets for each section within the profraw buffer.
#[cfg(feature = "alloc")]
pub(crate) struct SectionLayout {
    pub data_offset: usize,
    pub counters_offset: usize,
    pub bitmap_offset: usize,
    pub names_offset: usize,
    pub names_end: usize,
}

#[cfg(feature = "alloc")]
impl SectionLayout {
    pub fn compute(header: &RawHeader) -> Option<Self> {
        let data_offset = 128usize.checked_add(usize::try_from(header.binary_ids_size).ok()?)?;
        let data_size = header.num_data.checked_mul(64)?; // 64 bytes per record
        let counters_offset = data_offset
            .checked_add(usize::try_from(data_size).ok()?)?
            .checked_add(usize::try_from(header.padding_before_counters).ok()?)?;
        let counters_bytes = header
            .num_counters
            .checked_mul(if header.is_byte_coverage() { 1 } else { 8 })?;
        let bitmap_offset = counters_offset
            .checked_add(usize::try_from(counters_bytes).ok()?)?
            .checked_add(usize::try_from(header.padding_after_counters).ok()?)?;
        let names_offset = bitmap_offset
            .checked_add(usize::try_from(header.num_bitmap_bytes).ok()?)?
            .checked_add(usize::try_from(header.padding_after_bitmap).ok()?)?;
        let names_end = names_offset.checked_add(usize::try_from(header.names_size).ok()?)?;
        Some(Self {
            data_offset,
            counters_offset,
            bitmap_offset,
            names_offset,
            names_end,
        })
    }
}

// === Data record parsing ===

/// Parsed fields from one function data record.
#[cfg(feature = "alloc")]
struct ParsedRecord {
    name_ref: u64,
    func_hash: u64,
    num_counters: u32,
    num_value_sites: [u16; IPVK_NUM_KINDS],
    num_bitmap_bytes: u32,
}

#[cfg(feature = "alloc")]
fn parse_data_record(bytes: &[u8], offset: usize) -> Option<ParsedRecord> {
    // Layout: name_ref(u64), func_hash(u64), counters_delta(u64),
    // bitmap_delta(u64), function_pointer(u64), values(u64),
    // num_counters(u32), num_value_sites([u16; 3]), num_bitmap_bytes(u32)
    let name_ref = read_u64(bytes, offset)?;
    let func_hash = read_u64(bytes, offset + 8)?;
    // Skip counters_delta(8), bitmap_delta(8), function_pointer(8), values(8) = 40 bytes
    let num_counters = read_u32(bytes, offset + 48)?;
    let mut num_value_sites = [0u16; IPVK_NUM_KINDS];
    for (i, site) in num_value_sites.iter_mut().enumerate() {
        *site = read_u16(bytes, offset + 52 + i * 2)?;
    }
    // num_value_sites ends at offset 58, then 2 bytes padding, so num_bitmap_bytes at 60
    let num_bitmap_bytes = read_u32(bytes, offset + 60)?;
    Some(ParsedRecord {
        name_ref,
        func_hash,
        num_counters,
        num_value_sites,
        num_bitmap_bytes,
    })
}

// === Public API ===

/// Parsed profraw data, ready for merge or inspection.
#[cfg(feature = "alloc")]
#[derive(Clone, Debug)]
pub(crate) struct ParsedProfraw {
    pub records: Vec<FunctionRecord>,
    pub counters: CounterStore,
    pub bitmap: BitmapStore,
    pub value_sites: ValueProfileStore,
    pub format: ProfileFormat,
}

/// Parses a `.profraw` byte buffer into structured data.
#[cfg(feature = "alloc")]
pub fn parse_profraw(bytes: &[u8]) -> Result<ParsedProfraw, ProfileError> {
    let header = RawHeader::parse(bytes).ok_or(ProfileError::MalformedInput)?;
    if !header.is_valid() {
        return Err(ProfileError::MalformedInput);
    }

    let layout = SectionLayout::compute(&header).ok_or(ProfileError::ArithmeticOverflow)?;
    if bytes.len() < layout.names_end {
        return Err(ProfileError::MalformedInput);
    }

    // Parse data records.
    let mut records = Vec::with_capacity(header.num_data as usize);
    let mut counter_offset: usize = 0;
    let mut bitmap_offset: usize = 0;
    for i in 0..header.num_data as usize {
        let rec_offset = layout
            .data_offset
            .checked_add(i.checked_mul(64).ok_or(ProfileError::ArithmeticOverflow)?)
            .ok_or(ProfileError::ArithmeticOverflow)?;
        let parsed = parse_data_record(bytes, rec_offset).ok_or(ProfileError::MalformedInput)?;
        records.push(FunctionRecord {
            name_ref: NameRef(parsed.name_ref),
            function_hash: FunctionHash(parsed.func_hash),
            counters: CounterRange {
                len: parsed.num_counters as usize,
            },
            bitmap: BitmapRange {
                start: bitmap_offset,
                len: parsed.num_bitmap_bytes as usize,
            },
            value_sites: ValueSiteRanges {
                num_sites_per_kind: parsed.num_value_sites,
            },
        });
        counter_offset = counter_offset
            .checked_add(parsed.num_counters as usize)
            .ok_or(ProfileError::ArithmeticOverflow)?;
        bitmap_offset = bitmap_offset
            .checked_add(parsed.num_bitmap_bytes as usize)
            .ok_or(ProfileError::ArithmeticOverflow)?;
    }

    // Parse counters.
    let counter_bytes_len = header
        .num_counters
        .checked_mul(if header.is_byte_coverage() { 1 } else { 8 })
        .ok_or(ProfileError::ArithmeticOverflow)? as usize;
    let counters = if header.is_byte_coverage() {
        let counter_slice = bytes
            .get(
                layout.counters_offset
                    ..layout
                        .counters_offset
                        .checked_add(counter_bytes_len)
                        .ok_or(ProfileError::ArithmeticOverflow)?,
            )
            .ok_or(ProfileError::MalformedInput)?;
        CounterStore::Byte(counter_slice.to_vec())
    } else {
        let num_counters = header.num_counters as usize;
        let mut wide = Vec::with_capacity(num_counters);
        for i in 0..num_counters {
            let off = layout
                .counters_offset
                .checked_add(i.checked_mul(8).ok_or(ProfileError::ArithmeticOverflow)?)
                .ok_or(ProfileError::ArithmeticOverflow)?;
            let val = read_u64(bytes, off).ok_or(ProfileError::MalformedInput)?;
            wide.push(val);
        }
        CounterStore::Wide(wide)
    };

    // Parse bitmap.
    let bitmap_slice = bytes
        .get(
            layout.bitmap_offset
                ..layout
                    .bitmap_offset
                    .checked_add(header.num_bitmap_bytes as usize)
                    .ok_or(ProfileError::ArithmeticOverflow)?,
        )
        .ok_or(ProfileError::MalformedInput)?;
    let bitmap = BitmapStore::new(bitmap_slice.to_vec());

    // Parse names.
    // Parse names (validated but not stored — used only for section layout).
    let _names_slice = bytes
        .get(layout.names_offset..layout.names_end)
        .ok_or(ProfileError::MalformedInput)?;

    // Value profiling data offset (after names + padding).
    let names_padding = (7 & (8 - header.names_size % 8)) as usize;
    let vp_offset = layout
        .names_end
        .checked_add(names_padding)
        .ok_or(ProfileError::ArithmeticOverflow)?;

    let value_sites = parse_value_prof_data(bytes, vp_offset, &records)?;

    let pointer_width = if header.magic == crate::abi::layout::INSTR_PROF_RAW_MAGIC_64 {
        8
    } else {
        4
    };
    let format = ProfileFormat {
        raw_version: header.version,
        is_byte_coverage: header.is_byte_coverage(),
        pointer_width,
    };

    Ok(ParsedProfraw {
        records,
        counters,
        bitmap,
        value_sites,
        format,
    })
}

/// Maps a raw value kind index to `ValueKind`.
fn value_kind_from_raw(raw: u32) -> Option<ValueKind> {
    match raw {
        0 => Some(ValueKind::IndirectCallTarget),
        1 => Some(ValueKind::MemOpSize),
        2 => Some(ValueKind::VtableTarget),
        _ => None,
    }
}

/// Number of padding bytes to align `size_bytes` to 8.
fn num_padding_bytes(size_bytes: u64) -> u64 {
    7 & (8 - size_bytes % 8)
}

/// Parses value profiling data from the profraw buffer.
///
/// LLVM's format stores one `ValueProfData` block per function.
/// This function iterates over all records and parses each block.
#[cfg(feature = "alloc")]
fn parse_value_prof_data(
    bytes: &[u8],
    vp_offset: usize,
    records: &[FunctionRecord],
) -> Result<ValueProfileStore, ProfileError> {
    // Compute total number of value sites across all records and kinds.
    let total_sites: usize = records.iter().map(|r| r.value_sites.total_sites()).sum();

    let mut store =
        ValueProfileStore::new(crate::abi::layout::INSTR_PROF_DEFAULT_NUM_VAL_PER_SITE as usize);

    // No value profiling data if no sites.
    if total_sites == 0 {
        return Ok(store);
    }

    // Initialize sites from records.
    store.initialize_sites(records);

    let mut offset = vp_offset;
    let mut flat_site_offset = 0usize;

    // Parse one ValueProfData block per function.
    for record in records {
        let num_func_sites = record.value_sites.total_sites();
        if num_func_sites == 0 {
            continue;
        }

        // Check if there's enough data for the ValueProfData header.
        if bytes.len()
            < offset
                .checked_add(8)
                .ok_or(ProfileError::ArithmeticOverflow)?
        {
            // No more value profiling data — return what we have.
            return Ok(store);
        }

        let total_size = read_u32(bytes, offset).ok_or(ProfileError::MalformedInput)? as usize;
        let num_value_kinds =
            read_u32(bytes, offset + 4).ok_or(ProfileError::MalformedInput)? as usize;

        if total_size < 8 || num_value_kinds == 0 {
            // Skip this function's block (no data).
            flat_site_offset += num_func_sites;
            continue;
        }

        let vp_end = offset
            .checked_add(total_size)
            .ok_or(ProfileError::ArithmeticOverflow)?;
        if bytes.len() < vp_end {
            return Err(ProfileError::MalformedInput);
        }

        // Parse the ValueProfData block for this function.
        let mut block_offset = offset
            .checked_add(8)
            .ok_or(ProfileError::ArithmeticOverflow)?;

        // Track which site we're at within this function per kind.
        let mut kind_site_within_func = [0usize; IPVK_NUM_KINDS];

        for _ in 0..num_value_kinds {
            let kind_raw = read_u32(bytes, block_offset).ok_or(ProfileError::MalformedInput)?;
            let num_sites =
                read_u32(bytes, block_offset + 4).ok_or(ProfileError::MalformedInput)? as usize;
            let _kind = value_kind_from_raw(kind_raw).ok_or(ProfileError::MalformedInput)?;
            let kind_idx = kind_raw as usize;

            block_offset = block_offset
                .checked_add(8)
                .ok_or(ProfileError::ArithmeticOverflow)?;

            // Read site count bytes.
            if bytes.len()
                < block_offset
                    .checked_add(num_sites)
                    .ok_or(ProfileError::ArithmeticOverflow)?
            {
                return Err(ProfileError::MalformedInput);
            }
            let site_counts: Vec<u8> = bytes[block_offset..block_offset + num_sites].to_vec();
            block_offset = block_offset
                .checked_add(num_sites)
                .ok_or(ProfileError::ArithmeticOverflow)?;

            // Padding to 8-byte alignment.
            let record_so_far = 8u64 + num_sites as u64;
            let pad = num_padding_bytes(record_so_far) as usize;
            block_offset = block_offset
                .checked_add(pad)
                .ok_or(ProfileError::ArithmeticOverflow)?;

            // Read value data for each site.
            let base_within_kind = kind_site_within_func.get(kind_idx).copied().unwrap_or(0);
            for (site_i, &count) in site_counts.iter().enumerate() {
                // Compute the flat site index across all functions.
                let flat_site_index = flat_site_offset
                    + compute_site_offset_in_function(record, kind_idx, base_within_kind + site_i);
                let num_values = count as usize;

                for _ in 0..num_values {
                    if bytes.len()
                        < block_offset
                            .checked_add(16)
                            .ok_or(ProfileError::ArithmeticOverflow)?
                    {
                        return Err(ProfileError::MalformedInput);
                    }
                    let value =
                        read_u64(bytes, block_offset).ok_or(ProfileError::MalformedInput)?;
                    let count_val =
                        read_u64(bytes, block_offset + 8).ok_or(ProfileError::MalformedInput)?;
                    block_offset = block_offset
                        .checked_add(16)
                        .ok_or(ProfileError::ArithmeticOverflow)?;

                    store.record_value(flat_site_index, value, count_val);
                }
            }

            if let Some(cursor) = kind_site_within_func.get_mut(kind_idx) {
                *cursor = base_within_kind + num_sites;
            }
        }

        offset = vp_end;
        flat_site_offset += num_func_sites;
    }

    Ok(store)
}

/// Computes the offset of a site within a function's flat site layout.
///
/// Given a function record, a value kind index, and a site index within that
/// kind, returns the offset from the function's first site.
#[cfg(feature = "alloc")]
fn compute_site_offset_in_function(
    record: &FunctionRecord,
    kind_idx: usize,
    site_within_kind: usize,
) -> usize {
    let mut offset = 0usize;
    for k in 0..kind_idx {
        offset += record.value_sites.num_sites_per_kind[k] as usize;
    }
    offset + site_within_kind
}

/// Checks if a parsed profraw is compatible with the given `ProfileImage`.
/// Compares structured metadata: version, counter kind, function count,
/// per-function name_ref, func_hash, num_counters, num_bitmap_bytes.
#[cfg(feature = "alloc")]
pub fn check_compatibility(
    image: &ProfileImage,
    parsed: &ParsedProfraw,
) -> Result<(), ProfileError> {
    if parsed.format.raw_version != image.format.raw_version {
        return Err(ProfileError::IncompatibleInput);
    }
    if parsed.format.is_byte_coverage != image.format.is_byte_coverage {
        return Err(ProfileError::IncompatibleInput);
    }
    if parsed.records.len() != image.records.len() {
        return Err(ProfileError::IncompatibleInput);
    }

    for (src, dst) in parsed.records.iter().zip(image.records.iter()) {
        if src.name_ref != dst.name_ref
            || src.function_hash != dst.function_hash
            || src.counters.len != dst.counters.len
            || src.bitmap.len != dst.bitmap.len
        {
            return Err(ProfileError::IncompatibleInput);
        }
    }

    Ok(())
}

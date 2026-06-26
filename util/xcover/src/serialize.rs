// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Profraw serialization — encodes `ProfileSnapshot` into LLVM `.profraw` format.
//!
//! Pure safe Rust. No `unsafe`, no ABI layout types, no raw pointers.
//! All struct fields are manually serialized via `.to_le_bytes()`.

#[cfg(feature = "alloc")]
use alloc::vec::Vec;
use core::mem::size_of;

use crate::{
    ProfileError, ProfileWriter,
    abi::layout::IPVK_NUM_KINDS,
    image::{CounterStore, ProfileFormat, ProfileSnapshot, ValueKind, ValueSite},
};

// === Constants ===

/// Header: 16 × u64 = 128 bytes.
const HEADER_SIZE: u64 = 128;

/// Data record: mirrors sizeof(__llvm_profile_data) = 64 bytes.
const RECORD_SIZE: u64 = 64;

// === Padding helpers ===

/// Number of padding bytes to align `size_bytes` to 8.
fn num_padding_bytes(size_bytes: u64) -> u64 {
    7 & (8 - size_bytes % 8)
}

// === Magic ===

fn magic_for_format(format: &ProfileFormat) -> u64 {
    if format.pointer_width == 8 {
        crate::abi::layout::INSTR_PROF_RAW_MAGIC_64
    } else {
        crate::abi::layout::INSTR_PROF_RAW_MAGIC_32
    }
}

// === Counter helpers ===

fn counter_bytes_len(counters: &CounterStore) -> u64 {
    match counters {
        CounterStore::Wide(v) => v.len() as u64 * size_of::<u64>() as u64,
        CounterStore::Byte(v) => v.len() as u64,
    }
}

fn num_counters(counters: &CounterStore) -> u64 {
    match counters {
        CounterStore::Wide(v) => v.len() as u64,
        CounterStore::Byte(v) => v.len() as u64,
    }
}

fn counter_entry_size(format: &ProfileFormat) -> u64 {
    if format.is_byte_coverage { 1 } else { 8 }
}

// === Value kind mapping ===

fn value_kind_from_index(index: usize) -> ValueKind {
    match index {
        0 => ValueKind::IndirectCallTarget,
        1 => ValueKind::MemOpSize,
        _ => ValueKind::VtableTarget,
    }
}

// === Value prof size calculation ===

/// Value prof record header size: fixed(8) + site_counts + padding.
fn vp_record_header_size(num_sites: u32) -> u64 {
    let fixed = 8u64; // kind(u32) + num_value_sites(u32)
    let site_counts = num_sites as u64;
    let total = fixed + site_counts;
    total + num_padding_bytes(total)
}

/// Computes the byte size of ValueProfData for a single function's sites.
///
/// Returns 0 if the function has no active value kinds.
#[cfg(feature = "alloc")]
fn one_function_value_prof_data_size(sites: &[&ValueSite]) -> u64 {
    if sites.is_empty() {
        return 0;
    }

    let mut num_kinds = 0u32;
    let mut total_size = 8u64; // ValueProfData header: total_size(u32) + num_value_kinds(u32)

    for vk in 0..IPVK_NUM_KINDS {
        let kind = value_kind_from_index(vk);
        let mut kind_site_count = 0u32;
        let mut kind_value_bytes = 0u64;
        for site in sites {
            if site.kind == kind {
                kind_site_count += 1;
                kind_value_bytes += site.values.len() as u64 * 16;
            }
        }
        if kind_site_count == 0 {
            continue;
        }
        num_kinds += 1;
        total_size += vp_record_header_size(kind_site_count);
        total_size += kind_value_bytes;
    }

    if num_kinds == 0 { 0 } else { total_size }
}

/// Total byte size of value profiling data across all functions.
///
/// Iterates over records and their sites to compute per-function sizes.
#[cfg(feature = "alloc")]
fn value_prof_data_size(records: &[crate::record::FunctionRecord], sites: &[ValueSite]) -> u64 {
    if sites.is_empty() || records.is_empty() {
        return 0;
    }

    let mut total: u64 = 0;
    let mut site_offset = 0usize;

    for record in records {
        let num_sites = record.value_sites.total_sites();
        if num_sites == 0 {
            continue;
        }

        let func_sites: Vec<&ValueSite> = sites
            .get(site_offset..site_offset + num_sites)
            .map(|s| s.iter().collect())
            .unwrap_or_default();

        total += one_function_value_prof_data_size(&func_sites);
        site_offset += num_sites;
    }

    total
}

// === Public API ===

/// Computes the total serialized buffer size for the given snapshot.
#[cfg(feature = "alloc")]
pub fn encoded_size(snapshot: &ProfileSnapshot) -> u64 {
    let data_size = snapshot.records.len() as u64 * RECORD_SIZE;
    let counters_size = counter_bytes_len(&snapshot.counters);
    let bitmap_size = snapshot.bitmap.len() as u64;
    let names_size = snapshot.names.len() as u64;

    let padding_after_counters = num_padding_bytes(counters_size);
    let padding_after_bitmap = num_padding_bytes(bitmap_size);
    let padding_after_names = num_padding_bytes(names_size);

    HEADER_SIZE
        + data_size
        + counters_size
        + padding_after_counters
        + bitmap_size
        + padding_after_bitmap
        + names_size
        + padding_after_names
        + value_prof_data_size(&snapshot.records, snapshot.value_sites.sites())
}

/// Encodes a `ProfileSnapshot` into profraw bytes and writes via `ProfileWriter`.
#[cfg(feature = "alloc")]
pub fn encode(
    snapshot: &ProfileSnapshot,
    writer: &mut dyn ProfileWriter,
) -> Result<(), ProfileError> {
    let data_size = snapshot.records.len() as u64 * RECORD_SIZE;
    let counters_size = counter_bytes_len(&snapshot.counters);
    let bitmap_size = snapshot.bitmap.len() as u64;
    let names_size = snapshot.names.len() as u64;
    let n_counters = num_counters(&snapshot.counters);

    let padding_after_counters = num_padding_bytes(counters_size);
    let padding_after_bitmap = num_padding_bytes(bitmap_size);
    let padding_after_names = num_padding_bytes(names_size);

    // Compute section offsets from the beginning of the buffer.
    let counters_start = HEADER_SIZE + data_size;
    let bitmap_start = counters_start + counters_size + padding_after_counters;
    let names_start = bitmap_start + bitmap_size + padding_after_bitmap;

    // === Write header (16 × u64 = 128 bytes) as a single write ===
    let header_fields: [u64; 16] = [
        magic_for_format(&snapshot.format), // magic
        snapshot.format.raw_version,        // version
        0,                                  // binary_ids_size
        snapshot.records.len() as u64,      // num_data
        0,                                  // padding_bytes_before_counters
        n_counters,                         // num_counters
        padding_after_counters,             // padding_bytes_after_counters
        bitmap_size,                        // num_bitmap_bytes
        padding_after_bitmap,               // padding_bytes_after_bitmap_bytes
        names_size,                         // names_size
        if snapshot.records.is_empty() {
            0
        } else {
            counters_start.saturating_sub(HEADER_SIZE)
        }, // counters_delta
        if snapshot.records.is_empty() {
            0
        } else {
            bitmap_start.saturating_sub(HEADER_SIZE)
        }, // bitmap_delta
        if snapshot.records.is_empty() {
            0
        } else {
            names_start.saturating_sub(HEADER_SIZE)
        }, // names_delta
        0,                                  // num_vtables
        0,                                  // vnames_size
        crate::abi::layout::IPVK_LAST as u64, // value_kind_last
    ];
    let mut header_buf = [0u8; 128];
    for (i, &field) in header_fields.iter().enumerate() {
        header_buf[i * 8..i * 8 + 8].copy_from_slice(&field.to_le_bytes());
    }
    writer
        .write_all(&header_buf)
        .map_err(|_| ProfileError::WriterFailed)?;

    // === Write data records (pre-assembled 64-byte records) ===
    let mut counter_offset_in_section: u64 = 0;
    let mut bitmap_offset_in_section: u64 = 0;
    let entry_sz = counter_entry_size(&snapshot.format);

    for (i, record) in snapshot.records.iter().enumerate() {
        let record_file_offset = HEADER_SIZE + i as u64 * RECORD_SIZE;
        let counter_file_offset = counters_start + counter_offset_in_section;
        let bitmap_file_offset = bitmap_start + bitmap_offset_in_section;
        let counter_ptr = counter_file_offset
            .checked_sub(record_file_offset)
            .ok_or(ProfileError::ArithmeticOverflow)?;
        let bitmap_ptr = bitmap_file_offset
            .checked_sub(record_file_offset)
            .ok_or(ProfileError::ArithmeticOverflow)?;

        // Pre-assemble the 64-byte record into a stack buffer.
        let mut rec_buf = [0u8; 64];
        rec_buf[0..8].copy_from_slice(&record.name_ref.raw().to_le_bytes());
        rec_buf[8..16].copy_from_slice(&record.function_hash.raw().to_le_bytes());
        rec_buf[16..24].copy_from_slice(&counter_ptr.to_le_bytes());
        rec_buf[24..32].copy_from_slice(&bitmap_ptr.to_le_bytes());
        // function_pointer (32..40) and values (40..48) are zero — already zeroed
        rec_buf[48..52].copy_from_slice(&(record.counters.len as u32).to_le_bytes());
        for (k, &sites) in record.value_sites.num_sites_per_kind.iter().enumerate() {
            rec_buf[52 + k * 2..54 + k * 2].copy_from_slice(&sites.to_le_bytes());
        }
        rec_buf[60..64].copy_from_slice(&(record.bitmap.len as u32).to_le_bytes());

        writer
            .write_all(&rec_buf)
            .map_err(|_| ProfileError::WriterFailed)?;

        counter_offset_in_section = counter_offset_in_section
            .checked_add(record.counters.len as u64 * entry_sz)
            .ok_or(ProfileError::ArithmeticOverflow)?;
        bitmap_offset_in_section = bitmap_offset_in_section
            .checked_add(record.bitmap.len as u64)
            .ok_or(ProfileError::ArithmeticOverflow)?;
    }

    // === Write counters (batch for Wide mode) ===
    match &snapshot.counters {
        CounterStore::Wide(v) => {
            // Batch-convert all counters to bytes in one allocation.
            let mut buf = alloc::vec![0u8; v.len() * 8];
            for (i, &counter) in v.iter().enumerate() {
                buf[i * 8..i * 8 + 8].copy_from_slice(&counter.to_le_bytes());
            }
            writer
                .write_all(&buf)
                .map_err(|_| ProfileError::WriterFailed)?;
        }
        CounterStore::Byte(v) => {
            writer
                .write_all(v)
                .map_err(|_| ProfileError::WriterFailed)?;
        }
    }

    // === Write padding after counters ===
    write_zeroes(padding_after_counters as usize, writer)?;

    // === Write bitmap ===
    writer
        .write_all(snapshot.bitmap.as_slice())
        .map_err(|_| ProfileError::WriterFailed)?;

    // === Write padding after bitmap ===
    write_zeroes(padding_after_bitmap as usize, writer)?;

    // === Write names ===
    writer
        .write_all(snapshot.names.as_slice())
        .map_err(|_| ProfileError::WriterFailed)?;

    // === Write padding after names ===
    write_zeroes(padding_after_names as usize, writer)?;

    // === Write value profiling data (per-function) ===
    write_value_prof_data_per_function(&snapshot.records, snapshot.value_sites.sites(), writer)?;

    Ok(())
}

/// Writes `n` zero bytes.
fn write_zeroes(n: usize, writer: &mut dyn ProfileWriter) -> Result<(), ProfileError> {
    if n == 0 {
        return Ok(());
    }
    // Write in chunks of 8 bytes to avoid large allocations.
    let chunk = [0u8; 8];
    let full_chunks = n / 8;
    let remainder = n % 8;
    for _ in 0..full_chunks {
        writer
            .write_all(&chunk)
            .map_err(|_| ProfileError::WriterFailed)?;
    }
    writer
        .write_all(&chunk[..remainder])
        .map_err(|_| ProfileError::WriterFailed)?;
    Ok(())
}

/// Writes value profiling data for a single function in LLVM profraw format.
///
/// Mirrors `writeOneValueProfData` from LLVM's InstrProfilingWriter.c.
#[cfg(feature = "alloc")]
fn write_one_function_value_prof_data(
    func_sites: &[&ValueSite],
    writer: &mut dyn ProfileWriter,
) -> Result<(), ProfileError> {
    // Count active kinds for this function.
    let mut active_kinds: Vec<usize> = Vec::new();
    for vk in 0..IPVK_NUM_KINDS {
        let kind = value_kind_from_index(vk);
        let has_sites = func_sites.iter().any(|s| s.kind == kind);
        if has_sites {
            active_kinds.push(vk);
        }
    }

    if active_kinds.is_empty() {
        return Ok(());
    }

    // Compute total size.
    let mut total_size: u64 = 8; // ValueProfData header
    for &vk in &active_kinds {
        let kind = value_kind_from_index(vk);
        let kind_sites: Vec<&&ValueSite> = func_sites.iter().filter(|s| s.kind == kind).collect();
        let num_sites = kind_sites.len() as u32;
        total_size += vp_record_header_size(num_sites);
        for site in &kind_sites {
            total_size += site.values.len() as u64 * 16;
        }
    }

    // Write ValueProfData header.
    writer
        .write_all(&(total_size as u32).to_le_bytes())
        .map_err(|_| ProfileError::WriterFailed)?;
    writer
        .write_all(&(active_kinds.len() as u32).to_le_bytes())
        .map_err(|_| ProfileError::WriterFailed)?;

    // Write records for each active kind.
    for &vk in &active_kinds {
        let kind = value_kind_from_index(vk);
        let kind_sites: Vec<&&ValueSite> = func_sites.iter().filter(|s| s.kind == kind).collect();
        let num_sites = kind_sites.len() as u32;

        // Record header: kind(u32) + num_value_sites(u32).
        writer
            .write_all(&(vk as u32).to_le_bytes())
            .map_err(|_| ProfileError::WriterFailed)?;
        writer
            .write_all(&num_sites.to_le_bytes())
            .map_err(|_| ProfileError::WriterFailed)?;

        // Site count array: one byte per site.
        for site in &kind_sites {
            let count = site.values.len().min(255) as u8;
            writer
                .write_all(&[count])
                .map_err(|_| ProfileError::WriterFailed)?;
        }

        // Padding to 8-byte alignment.
        let record_so_far = 8u64 + num_sites as u64; // fixed header + site counts
        let pad = num_padding_bytes(record_so_far);
        write_zeroes(pad as usize, writer)?;

        // Value data for each site.
        for site in &kind_sites {
            for vc in &site.values {
                writer
                    .write_all(&vc.value.to_le_bytes())
                    .map_err(|_| ProfileError::WriterFailed)?;
                writer
                    .write_all(&vc.count.to_le_bytes())
                    .map_err(|_| ProfileError::WriterFailed)?;
            }
        }
    }

    Ok(())
}

/// Writes value profiling data for all functions in LLVM profraw format.
///
/// Each function gets its own `ValueProfData` block, matching LLVM's format.
#[cfg(feature = "alloc")]
fn write_value_prof_data_per_function(
    records: &[crate::record::FunctionRecord],
    sites: &[ValueSite],
    writer: &mut dyn ProfileWriter,
) -> Result<(), ProfileError> {
    if sites.is_empty() || records.is_empty() {
        return Ok(());
    }

    let mut site_offset = 0usize;

    for record in records {
        let num_sites = record.value_sites.total_sites();
        if num_sites == 0 {
            continue;
        }

        // Gather this function's sites.
        let func_sites: Vec<&ValueSite> = sites
            .get(site_offset..site_offset + num_sites)
            .map(|s| s.iter().collect())
            .unwrap_or_default();

        write_one_function_value_prof_data(&func_sites, writer)?;
        site_offset += num_sites;
    }

    Ok(())
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Profile data merging.
//!
//! Merges a `ParsedProfraw` into a live `ProfileImage` using structured data.
//! No unsafe, no ABI types, no raw pointers.
//!
//! Merge rules:
//! - Wide counters: `saturating_add` via atomic CAS
//! - Byte counters: bitwise AND via atomic fetch_and
//! - Bitmap: bitwise OR via atomic fetch_or
//! - Value sites: per-site atomic accumulate
//!
//! The live `ProfileImage` exposes only `&self`; all mutations go through
//! atomic operations.

use crate::{
    ProfileError,
    image::{AtomicCounterStore, CounterStore, ProfileImage},
    parse::ParsedProfraw,
};

/// Merges parsed profraw data into the given profile image.
///
/// The caller must ensure compatibility (same version, same records)
/// before calling this function.
pub fn merge(image: &ProfileImage, parsed: &ParsedProfraw) -> Result<(), ProfileError> {
    check_counter_lengths(&image.counters, &parsed.counters)?;

    image.counters.merge_from(&parsed.counters);

    let min_bitmap = image.bitmap.len().min(parsed.bitmap.len());
    image.bitmap.or_assign(
        crate::record::BitmapRange {
            start: 0,
            len: min_bitmap,
        },
        &parsed.bitmap.as_slice()[..min_bitmap],
    );

    image.value_sites.merge_from(&parsed.value_sites);

    Ok(())
}

/// Verifies that destination (atomic) and source (parsed) counter stores
/// have matching length and kind before merging.
fn check_counter_lengths(dst: &AtomicCounterStore, src: &CounterStore) -> Result<(), ProfileError> {
    match (dst, src) {
        (AtomicCounterStore::Wide(d), CounterStore::Wide(s)) => {
            if d.len() != s.len() {
                return Err(ProfileError::IncompatibleInput);
            }
        }
        (AtomicCounterStore::Byte(d), CounterStore::Byte(s)) => {
            if d.len() != s.len() {
                return Err(ProfileError::IncompatibleInput);
            }
        }
        _ => return Err(ProfileError::IncompatibleInput),
    }
    Ok(())
}

/// Computes a load module signature from a `ProfileImage`.
/// Mirrors `lprofGetLoadModuleSignature` from LLVM compiler-rt:
///   (NamesSize << 40) + (NumCounters << 30) + (NumData << 20) +
///   (NumVnodes << 10) + FirstD->NameRef + Version + Magic
pub fn get_load_module_signature(image: &ProfileImage) -> u64 {
    let names_size = image.names.len() as u64;
    let num_counters = image.counters.len() as u64;
    let num_data = image.records.len() as u64;
    let num_vnodes = image.value_sites.total_value_count() as u64;
    let first_name_ref = if num_data > 0 {
        image.records.first().map_or(0, |r| r.name_ref.raw())
    } else {
        0
    };
    let magic = if image.format.pointer_width == 8 {
        crate::abi::layout::INSTR_PROF_RAW_MAGIC_64
    } else {
        crate::abi::layout::INSTR_PROF_RAW_MAGIC_32
    };

    (names_size << 40)
        .wrapping_add(num_counters << 30)
        .wrapping_add(num_data << 20)
        .wrapping_add(num_vnodes << 10)
        .wrapping_add(first_name_ref)
        .wrapping_add(image.format.raw_version)
        .wrapping_add(magic)
}

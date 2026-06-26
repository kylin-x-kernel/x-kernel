// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Profile data merging.
//!
//! Merges a `ParsedProfraw` into a `ProfileImage` using structured data.
//! No unsafe, no ABI types, no raw pointers.
//!
//! Merge rules:
//! - Wide counters: `saturating_add`
//! - Byte counters: bitwise AND
//! - Bitmap: bitwise OR
//! - Value sites: accumulate counts per site

use crate::{
    ProfileError,
    image::{CounterStore, ProfileImage},
    parse::ParsedProfraw,
};

/// Merges parsed profraw data into the given profile image.
///
/// The caller must ensure compatibility (same version, same records)
/// before calling this function.
pub fn merge(image: &mut ProfileImage, parsed: &ParsedProfraw) -> Result<(), ProfileError> {
    // Merge counters.
    merge_counters(&mut image.counters, &parsed.counters)?;

    // Merge bitmap.
    merge_bitmap(&mut image.bitmap, &parsed.bitmap);

    // Merge value profiling data.
    merge_value_sites(&mut image.value_sites, &parsed.value_sites);

    Ok(())
}

/// Merges source counters into destination counters.
pub(crate) fn merge_counters(
    dst: &mut CounterStore,
    src: &CounterStore,
) -> Result<(), ProfileError> {
    match (dst, src) {
        (CounterStore::Wide(dst_vec), CounterStore::Wide(src_vec)) => {
            if dst_vec.len() != src_vec.len() {
                return Err(ProfileError::IncompatibleInput);
            }
            for (d, s) in dst_vec.iter_mut().zip(src_vec.iter()) {
                *d = d.saturating_add(*s);
            }
        }
        (CounterStore::Byte(dst_vec), CounterStore::Byte(src_vec)) => {
            if dst_vec.len() != src_vec.len() {
                return Err(ProfileError::IncompatibleInput);
            }
            for (d, s) in dst_vec.iter_mut().zip(src_vec.iter()) {
                *d &= *s;
            }
        }
        _ => return Err(ProfileError::IncompatibleInput),
    }
    Ok(())
}

/// Merges source bitmap into destination bitmap via bitwise OR.
fn merge_bitmap(dst: &mut crate::image::BitmapStore, src: &crate::image::BitmapStore) {
    let dst_slice = dst.as_slice();
    let src_slice = src.as_slice();
    // Use or_assign with a full-range BitmapRange.
    let range = crate::record::BitmapRange {
        start: 0,
        len: dst_slice.len().min(src_slice.len()),
    };
    let src_bytes = &src_slice[..range.len.min(src_slice.len())];
    dst.or_assign(range, src_bytes);
}

/// Merges source value sites into destination by accumulating counts.
fn merge_value_sites(
    dst: &mut crate::image::ValueProfileStore,
    src: &crate::image::ValueProfileStore,
) {
    dst.merge_from(src);
}

/// Computes a load module signature from a `ProfileImage`.
/// Mirrors `lprofGetLoadModuleSignature` from LLVM compiler-rt:
///   (NamesSize << 40) + (NumCounters << 30) + (NumData << 20) +
///   (NumVnodes << 10) + FirstD->NameRef + Version + Magic
pub fn get_load_module_signature(image: &ProfileImage) -> u64 {
    let names_size = image.names.len() as u64;
    let num_counters = match &image.counters {
        CounterStore::Wide(v) => v.len() as u64,
        CounterStore::Byte(v) => v.len() as u64,
    };
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

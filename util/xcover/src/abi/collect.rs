// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linker section collection -> ProfileImage.
//!
//! This is the **only** place in the crate that reads raw linker section
//! pointers. All unsafe pointer operations for section collection live here.

#[cfg(feature = "alloc")]
use alloc::vec::Vec;
use core::mem::size_of;

#[cfg(feature = "alloc")]
use crate::ProfileError;
use crate::abi::layout::*;
#[cfg(feature = "alloc")]
use crate::image::*;
#[cfg(feature = "alloc")]
use crate::record::*;

/// Collects profile data from linker sections into an owned `ProfileImage`.
///
/// Must be called after the linker has resolved section boundary symbols.
/// The caller must guarantee no concurrent mutation of the profile sections
/// (e.g., by holding the profile sections lock).
#[cfg(feature = "alloc")]
pub(crate) fn collect_profile_image() -> Result<ProfileImage, ProfileError> {
    let sections = crate::platform::profile_sections();
    let version = INSTR_PROF_RAW_VERSION;
    let is_byte_coverage = version & VARIANT_MASK_BYTE_COVERAGE != 0;

    let records = collect_records(sections.data.as_slice())?;
    let counters = collect_counters(sections.counters.as_slice(), is_byte_coverage)?;
    let bitmap = BitmapStore::new(sections.bitmap.as_slice().to_vec());
    let names = NameTable::new(sections.names.as_slice().to_vec());
    let mut value_sites = ValueProfileStore::new(INSTR_PROF_DEFAULT_NUM_VAL_PER_SITE as usize);
    value_sites.initialize_sites(&records);

    let format = ProfileFormat {
        raw_version: version,
        is_byte_coverage,
        pointer_width: size_of::<*const ()>(),
    };

    Ok(ProfileImage {
        records,
        counters,
        bitmap,
        names,
        value_sites,
        format,
    })
}

#[cfg(feature = "alloc")]
fn collect_records(data: &[LlvmProfileData]) -> Result<Vec<FunctionRecord>, ProfileError> {
    let mut records = Vec::with_capacity(data.len());
    let mut counter_offset = 0usize;
    let mut bitmap_offset = 0usize;

    for entry in data {
        let record = FunctionRecord {
            name_ref: NameRef(entry.name_ref),
            function_hash: FunctionHash(entry.func_hash),
            counters: CounterRange {
                len: entry.num_counters as usize,
            },
            bitmap: BitmapRange {
                start: bitmap_offset,
                len: entry.num_bitmap_bytes as usize,
            },
            value_sites: ValueSiteRanges {
                num_sites_per_kind: entry.num_value_sites,
            },
        };
        counter_offset = counter_offset
            .checked_add(entry.num_counters as usize)
            .ok_or(ProfileError::ArithmeticOverflow)?;
        bitmap_offset = bitmap_offset
            .checked_add(entry.num_bitmap_bytes as usize)
            .ok_or(ProfileError::ArithmeticOverflow)?;
        records.push(record);
    }

    Ok(records)
}

#[cfg(feature = "alloc")]
fn collect_counters(counters: &[u8], is_byte_coverage: bool) -> Result<CounterStore, ProfileError> {
    if is_byte_coverage {
        Ok(CounterStore::Byte(counters.to_vec()))
    } else {
        let entry_size = size_of::<u64>();
        if !counters.len().is_multiple_of(entry_size) {
            return Err(ProfileError::MalformedInput);
        }
        let num_counters = counters.len() / entry_size;
        let mut wide = Vec::with_capacity(num_counters);
        for i in 0..num_counters {
            let offset = i * entry_size;
            let bytes = &counters[offset..offset + entry_size];
            wide.push(u64::from_ne_bytes(
                bytes.try_into().map_err(|_| ProfileError::MalformedInput)?,
            ));
        }
        Ok(CounterStore::Wide(wide))
    }
}

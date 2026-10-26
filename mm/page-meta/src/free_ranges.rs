// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Normalize caller-owned free physical ranges and carve out metadata backing.

use memaddr::{PAGE_SIZE_4K, PhysAddr, PhysAddrRange};

use crate::PageMetaError;

fn complete_pages(range: PhysAddrRange) -> Result<PhysAddrRange, PageMetaError> {
    let start = range.start.as_usize();
    let end = range.end.as_usize();
    if start > end {
        return Err(PageMetaError::InvalidRange);
    }
    if start == end {
        return Ok(range);
    }
    let start = start
        .checked_add(PAGE_SIZE_4K - 1)
        .ok_or(PageMetaError::InvalidRange)?
        & !(PAGE_SIZE_4K - 1);
    let end = end & !(PAGE_SIZE_4K - 1);
    Ok(PhysAddrRange::new(start.into(), end.max(start).into()))
}

pub(crate) fn normalize(ranges: &mut [PhysAddrRange]) -> Result<(), PageMetaError> {
    for range in ranges.iter_mut() {
        *range = complete_pages(*range)?;
    }
    ranges.sort_unstable_by_key(|r| r.start);
    let mut count = 0;
    for index in 0..ranges.len() {
        let range = ranges[index];
        if range.is_empty() {
            continue;
        }
        if count != 0 {
            let previous = &mut ranges[count - 1];
            if range.start < previous.end {
                return Err(PageMetaError::Overlap);
            }
            if range.start == previous.end {
                previous.end = range.end;
                continue;
            }
        }
        ranges[count] = range;
        count += 1;
    }
    if count == 0 {
        return Err(PageMetaError::EmptyCoverage);
    }
    ranges[count..].fill(PhysAddrRange::new(
        PhysAddr::from_usize(0),
        PhysAddr::from_usize(0),
    ));
    Ok(())
}

// Commit the inventory change only after the backend has accepted the storage.
// This keeps failed mapping/initialization requests from consuming the range.
pub(crate) fn with_reserved_storage<T>(
    ranges: &mut [PhysAddrRange],
    bytes: usize,
    initialize: impl FnOnce(PhysAddrRange) -> Result<T, PageMetaError>,
) -> Result<T, PageMetaError> {
    let bytes = bytes
        .checked_add(PAGE_SIZE_4K - 1)
        .ok_or(PageMetaError::InvalidRange)?
        & !(PAGE_SIZE_4K - 1);
    if bytes == 0 {
        return Err(PageMetaError::InvalidRange);
    }
    let index = ranges
        .iter()
        .enumerate()
        .filter(|(_, r)| r.size() >= bytes)
        .max_by_key(|(_, r)| r.size())
        .map(|(index, _)| index)
        .ok_or(PageMetaError::NoMemory)?;
    let end = ranges[index].end;
    let backing = PhysAddrRange::new(end - bytes, end);
    let result = initialize(backing)?;
    ranges[index].end = backing.start;
    Ok(result)
}

unittest::path_mod_test!(tests, "../ut-tests/free_ranges.rs");

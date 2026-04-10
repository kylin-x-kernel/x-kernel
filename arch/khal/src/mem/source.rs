// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{
    ops::Range,
    sync::atomic::{AtomicUsize, Ordering},
};

use lazyinit::LazyInit;

use super::{MemRange, ReservedRegion};

const MAX_DESCRIBED_RAM_REGIONS: usize = 128;
const MAX_DESCRIBED_RESERVED_REGIONS: usize = 128;

static DESCRIBED_RAM_COUNT: AtomicUsize = AtomicUsize::new(0);
static DESCRIBED_RAM_REGIONS: LazyInit<[MemRange; MAX_DESCRIBED_RAM_REGIONS]> = LazyInit::new();
static DESCRIBED_RESERVED_COUNT: AtomicUsize = AtomicUsize::new(0);
static DESCRIBED_RESERVED_REGIONS: LazyInit<[ReservedRegion; MAX_DESCRIBED_RESERVED_REGIONS]> =
    LazyInit::new();

pub(crate) fn init_described_regions(
    ram_regions: &[MemRange],
    reserved_regions: &[ReservedRegion],
) {
    assert!(
        ram_regions.len() <= MAX_DESCRIBED_RAM_REGIONS,
        "too many described RAM regions"
    );
    assert!(
        reserved_regions.len() <= MAX_DESCRIBED_RESERVED_REGIONS,
        "too many described reserved regions"
    );

    let mut ram = [(0, 0); MAX_DESCRIBED_RAM_REGIONS];
    ram[..ram_regions.len()].copy_from_slice(ram_regions);
    let mut reserved = [ReservedRegion::EMPTY; MAX_DESCRIBED_RESERVED_REGIONS];
    reserved[..reserved_regions.len()].copy_from_slice(reserved_regions);

    DESCRIBED_RAM_REGIONS.init_once(ram);
    DESCRIBED_RESERVED_REGIONS.init_once(reserved);
    DESCRIBED_RAM_COUNT.store(ram_regions.len(), Ordering::SeqCst);
    DESCRIBED_RESERVED_COUNT.store(reserved_regions.len(), Ordering::SeqCst);
}

pub(crate) fn has_described_regions() -> bool {
    DESCRIBED_RAM_REGIONS.get().is_some()
}

pub(crate) fn described_ram_regions() -> &'static [MemRange] {
    let count = DESCRIBED_RAM_COUNT.load(Ordering::Relaxed);
    DESCRIBED_RAM_REGIONS
        .get()
        .map(|regions| &regions[..count])
        .expect("described RAM regions are not initialized")
}

pub(crate) fn described_reserved_regions() -> &'static [ReservedRegion] {
    let count = DESCRIBED_RESERVED_COUNT.load(Ordering::Relaxed);
    if count == 0 {
        return &[];
    }
    DESCRIBED_RESERVED_REGIONS
        .get()
        .map(|regions| &regions[..count])
        .expect("described reserved regions are not initialized")
}

pub fn total_ram() -> usize {
    described_ram_regions().iter().map(|r| r.1).sum()
}

pub type OverlapError = (Range<usize>, Range<usize>);

pub fn check_overlap(iter: impl Iterator<Item = MemRange>) -> Result<(), OverlapError> {
    let mut last = Range::default();
    for (s, n) in iter {
        if last.end > s {
            return Err((last, s..s + n));
        }
        last = s..s + n;
    }
    Ok(())
}

pub fn sub_ranges<F>(base: &[MemRange], cut: &[MemRange], mut cb: F) -> Result<(), OverlapError>
where
    F: FnMut(MemRange),
{
    check_overlap(cut.iter().cloned())?;

    for &(mut s, n) in base {
        let e = s + n;

        for &(cs, cn) in cut {
            let ce = cs + cn;
            if ce <= s {
                continue;
            }
            if cs >= e {
                break;
            }
            if cs > s {
                cb((s, cs - s));
            }
            s = ce;
        }
        if s < e {
            cb((s, e - s));
        }
    }
    Ok(())
}

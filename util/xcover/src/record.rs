// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Structured function record types.
//!
//! These types replace raw ABI struct fields with semantic newtypes,
//! preventing misuse (e.g., confusing a `name_ref` with a plain `u64`).

use crate::abi::layout::IPVK_NUM_KINDS;

/// Function name reference token. Not a bare `u64`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct NameRef(pub u64);

impl NameRef {
    pub fn raw(self) -> u64 {
        self.0
    }
}

/// Function hash. Not interchangeable with plain integers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct FunctionHash(pub u64);

impl FunctionHash {
    pub fn raw(self) -> u64 {
        self.0
    }
}

/// Counter range token. Only usable with `CounterStore`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CounterRange {
    pub len: usize,
}

/// Bitmap range token. Only usable with `BitmapStore`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BitmapRange {
    pub start: usize,
    pub len: usize,
}

/// Value profiling site counts per kind.
#[derive(Clone, Debug, Default)]
pub(crate) struct ValueSiteRanges {
    pub num_sites_per_kind: [u16; IPVK_NUM_KINDS],
}

impl ValueSiteRanges {
    /// Returns the total number of value sites across all kinds.
    pub fn total_sites(&self) -> usize {
        self.num_sites_per_kind.iter().map(|&n| n as usize).sum()
    }
}

/// Semantic function record — not an ABI struct.
///
/// Merge compatibility compares these semantic fields rather than
/// raw ABI struct bytes.
#[derive(Clone, Debug)]
pub(crate) struct FunctionRecord {
    pub name_ref: NameRef,
    pub function_hash: FunctionHash,
    pub counters: CounterRange,
    pub bitmap: BitmapRange,
    pub value_sites: ValueSiteRanges,
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Core data model: ProfileImage and its components.
//!
//! `ProfileImage` is the central structured data type. All core algorithms
//! (serialize, parse, merge, reset) operate on it, not on raw ABI pointers.

#[cfg(feature = "alloc")]
use alloc::vec::Vec;

use crate::record::{BitmapRange, FunctionRecord};

/// Counter storage. Byte coverage and regular 64-bit counters are two
/// distinct branches — no raw `&mut [u8]` interpretation needed.
#[cfg(feature = "alloc")]
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CounterStore {
    Wide(Vec<u64>),
    Byte(Vec<u8>),
}

#[cfg(feature = "alloc")]
impl CounterStore {
    /// Reset all counters to their initial value.
    pub fn reset(&mut self) {
        match self {
            CounterStore::Wide(v) => v.fill(0),
            CounterStore::Byte(v) => v.fill(0xFF), // byte coverage: 0xFF = "not covered"
        }
    }
}

/// Bitmap storage. Only exposes `or_assign` for merging — no arbitrary
/// mutable slice access.
#[cfg(feature = "alloc")]
#[derive(Clone, Debug)]
pub(crate) struct BitmapStore {
    bytes: Vec<u8>,
}

#[cfg(feature = "alloc")]
impl BitmapStore {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    /// Bitmap merge: bitwise OR within a range.
    pub fn or_assign(&mut self, range: BitmapRange, source: &[u8]) {
        for (i, &src_byte) in source.iter().enumerate() {
            if let Some(dst) = self.bytes.get_mut(range.start + i) {
                *dst |= src_byte;
            }
        }
    }

    /// Reset bitmap.
    pub fn clear(&mut self) {
        self.bytes.fill(0);
    }
}

/// Name data table. Only exposes byte range and hash/name ref queries.
#[cfg(feature = "alloc")]
#[derive(Clone, Debug)]
pub(crate) struct NameTable {
    bytes: Vec<u8>,
}

#[cfg(feature = "alloc")]
impl NameTable {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.bytes
    }
}

/// Value profiling kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ValueKind {
    IndirectCallTarget,
    MemOpSize,
    VtableTarget,
}

/// A single observed value and its count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ValueCount {
    pub value: u64,
    pub count: u64,
}

/// A value profiling site — a list of observed values, not a C linked list.
#[cfg(feature = "alloc")]
#[derive(Clone, Debug)]
pub(crate) struct ValueSite {
    pub kind: ValueKind,
    pub values: Vec<ValueCount>,
}

/// Value profiling store. Replaces C-style intrusive linked lists with
/// `Vec<ValueCount>` per site.
#[cfg(feature = "alloc")]
#[derive(Clone, Debug)]
pub(crate) struct ValueProfileStore {
    sites: Vec<ValueSite>,
    max_values_per_site: usize,
}

#[cfg(feature = "alloc")]
impl ValueProfileStore {
    pub fn new(max_values_per_site: usize) -> Self {
        Self {
            sites: Vec::new(),
            max_values_per_site,
        }
    }

    /// Initializes value profiling sites from function records.
    ///
    /// Creates empty `ValueSite` entries for each function's value profiling
    /// sites based on `num_sites_per_kind`. This must be called after parsing
    /// function records to enable value recording.
    pub fn initialize_sites(&mut self, records: &[FunctionRecord]) {
        self.sites.clear();

        for record in records {
            for (kind_idx, &num_sites) in record.value_sites.num_sites_per_kind.iter().enumerate() {
                let kind = match kind_idx {
                    0 => ValueKind::IndirectCallTarget,
                    1 => ValueKind::MemOpSize,
                    2 => ValueKind::VtableTarget,
                    _ => continue,
                };
                for _ in 0..num_sites {
                    self.sites.push(ValueSite {
                        kind,
                        values: Vec::new(),
                    });
                }
            }
        }
    }

    pub fn sites(&self) -> &[ValueSite] {
        &self.sites
    }

    /// Returns the total number of value entries across all sites.
    /// Mirrors the vnode count in LLVM's `lprofGetLoadModuleSignature`.
    pub fn total_value_count(&self) -> usize {
        self.sites.iter().map(|s| s.values.len()).sum()
    }

    /// Record a value observation. Pure safe logic — no linked list traversal.
    pub fn record_value(&mut self, site_index: usize, value: u64, count: u64) {
        if count == 0 {
            return;
        }
        let Some(site) = self.sites.get_mut(site_index) else {
            return;
        };

        // If this value already exists, accumulate the count.
        for vc in &mut site.values {
            if vc.value == value {
                vc.count = vc.count.saturating_add(count);
                return;
            }
        }

        // Site not full — push new entry.
        if site.values.len() < self.max_values_per_site {
            site.values.push(ValueCount { value, count });
            return;
        }

        // Site full — min-count replacement policy.
        let min_idx = site
            .values
            .iter()
            .enumerate()
            .min_by_key(|(_, vc)| vc.count)
            .map(|(i, _)| i);
        if let Some(idx) = min_idx {
            let min_count = site.values[idx].count;
            if min_count <= count {
                site.values[idx] = ValueCount { value, count };
            } else {
                site.values[idx].count = site.values[idx].count.saturating_sub(count);
            }
        }
    }

    /// Clear all counts, keeping the site structure.
    pub fn clear_counts(&mut self) {
        for site in &mut self.sites {
            for vc in &mut site.values {
                vc.count = 0;
            }
        }
    }

    /// Merges another store's value data into this one.
    /// For each site, accumulates counts for matching values.
    pub fn merge_from(&mut self, other: &ValueProfileStore) {
        for (i, src_site) in other.sites.iter().enumerate() {
            if i >= self.sites.len() {
                break;
            }
            for vc in &src_site.values {
                self.record_value(i, vc.value, vc.count);
            }
        }
    }
}

/// Profile format metadata.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ProfileFormat {
    pub raw_version: u64,
    pub is_byte_coverage: bool,
    pub pointer_width: usize,
}

/// Complete profile image — the core data model.
///
/// All core algorithms (serialize, parse, merge, reset) operate on this
/// structured type, never on raw ABI pointers.
#[cfg(feature = "alloc")]
#[derive(Clone, Debug)]
pub(crate) struct ProfileImage {
    pub records: Vec<FunctionRecord>,
    pub counters: CounterStore,
    pub bitmap: BitmapStore,
    pub names: NameTable,
    pub value_sites: ValueProfileStore,
    pub format: ProfileFormat,
}

/// Immutable snapshot for serialization. Avoids holding runtime locks
/// during I/O.
#[cfg(feature = "alloc")]
#[derive(Clone, Debug)]
pub struct ProfileSnapshot {
    pub(crate) records: Vec<FunctionRecord>,
    pub(crate) counters: CounterStore,
    pub(crate) bitmap: BitmapStore,
    pub(crate) names: NameTable,
    pub(crate) value_sites: ValueProfileStore,
    pub(crate) format: ProfileFormat,
}

#[cfg(feature = "alloc")]
impl ProfileImage {
    /// Create an immutable snapshot for serialization.
    pub fn snapshot(&self) -> ProfileSnapshot {
        ProfileSnapshot {
            records: self.records.clone(),
            counters: self.counters.clone(),
            bitmap: self.bitmap.clone(),
            names: self.names.clone(),
            value_sites: self.value_sites.clone(),
            format: self.format,
        }
    }
}

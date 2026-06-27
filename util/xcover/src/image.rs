// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Core data model: ProfileImage and its components.
//!
//! Two parallel hierarchies exist:
//! * Non-atomic types (`CounterStore`, `BitmapStore`, `ValueSite`, …) — used
//!   by `ProfileSnapshot` (immutable serialization input) and `ParsedProfraw`
//!   (parsed from external profraw input).
//! * Atomic types (`AtomicCounterStore`, `AtomicBitmapStore`, …) — used by
//!   the live `ProfileImage` held in `Runtime`. All mutation goes through
//!   `&self` and atomic operations; no `&mut` aliasing.

#[cfg(feature = "alloc")]
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use portable_atomic::{AtomicU8, AtomicU64, AtomicUsize};

use crate::record::{BitmapRange, FunctionRecord};

// === Non-atomic types (used by ProfileSnapshot / ParsedProfraw) ===

/// Counter storage. Byte coverage and regular 64-bit counters are two
/// distinct branches — no raw `&mut [u8]` interpretation needed.
#[cfg(feature = "alloc")]
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CounterStore {
    Wide(Vec<u64>),
    Byte(Vec<u8>),
}

/// Bitmap storage. Immutable snapshot type; mutation lives on
/// [`AtomicBitmapStore`].
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

    /// Record a value observation into a non-atomic store (used by the
    /// parser when constructing `ParsedProfraw`). Pure safe logic.
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
}

// === Atomic types (used by live ProfileImage in Runtime) ===

/// Atomic counter storage. Allows `&self` mutation via atomic ops.
#[cfg(feature = "alloc")]
#[derive(Debug)]
pub(crate) enum AtomicCounterStore {
    Wide(Vec<AtomicU64>),
    Byte(Vec<AtomicU8>),
}

#[cfg(feature = "alloc")]
impl AtomicCounterStore {
    /// Builds an atomic counter store from raw section bytes.
    /// `counters` slice is the raw `__llvm_prf_cnts` section content.
    pub fn from_section(counters: &[u8], is_byte_coverage: bool) -> Self {
        if is_byte_coverage {
            Self::Byte(counters.iter().map(|&b| AtomicU8::new(b)).collect())
        } else {
            let entry_size = core::mem::size_of::<u64>();
            let num = counters.len() / entry_size;
            let mut wide = Vec::with_capacity(num);
            for i in 0..num {
                let bytes = &counters[i * entry_size..(i + 1) * entry_size];
                let arr: [u8; 8] = bytes.try_into().unwrap_or([0; 8]);
                wide.push(AtomicU64::new(u64::from_ne_bytes(arr)));
            }
            Self::Wide(wide)
        }
    }

    /// Resets all counters to their initial value.
    pub fn reset(&self) {
        match self {
            Self::Wide(v) => v.iter().for_each(|c| c.store(0, Ordering::Relaxed)),
            // Byte coverage: 0xFF = "not covered".
            Self::Byte(v) => v.iter().for_each(|c| c.store(0xFF, Ordering::Relaxed)),
        }
    }

    /// Merges source counters into self via atomic fetch_*.
    /// Wide: saturating add. Byte: bitwise AND.
    pub fn merge_from(&self, src: &CounterStore) {
        match (self, src) {
            (Self::Wide(dst), CounterStore::Wide(s)) => {
                for (d, x) in dst.iter().zip(s.iter()) {
                    let mut current = d.load(Ordering::Relaxed);
                    loop {
                        let next = current.saturating_add(*x);
                        match d.compare_exchange_weak(
                            current,
                            next,
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                        ) {
                            Ok(_) => break,
                            Err(actual) => current = actual,
                        }
                    }
                }
            }
            (Self::Byte(dst), CounterStore::Byte(s)) => {
                for (d, x) in dst.iter().zip(s.iter()) {
                    // Byte coverage merge is AND (intersection of covered bytes).
                    d.fetch_and(*x, Ordering::Relaxed);
                }
            }
            _ => {}
        }
    }

    /// Atomically reads all counters into a non-atomic `CounterStore` snapshot.
    pub fn snapshot(&self) -> CounterStore {
        match self {
            Self::Wide(v) => {
                let mut out = Vec::with_capacity(v.len());
                for c in v.iter() {
                    out.push(c.load(Ordering::Relaxed));
                }
                CounterStore::Wide(out)
            }
            Self::Byte(v) => {
                let mut out = Vec::with_capacity(v.len());
                for c in v.iter() {
                    out.push(c.load(Ordering::Relaxed));
                }
                CounterStore::Byte(out)
            }
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::Wide(v) => v.len(),
            Self::Byte(v) => v.len(),
        }
    }
}

/// Atomic bitmap storage. `&self` mutation via atomic fetch_or.
#[cfg(feature = "alloc")]
#[derive(Debug)]
pub(crate) struct AtomicBitmapStore {
    bytes: Vec<AtomicU8>,
}

#[cfg(feature = "alloc")]
impl AtomicBitmapStore {
    pub fn from_slice(bytes: &[u8]) -> Self {
        Self {
            bytes: bytes.iter().map(|&b| AtomicU8::new(b)).collect(),
        }
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Bitmap merge: bitwise OR within a range, atomic.
    pub fn or_assign(&self, range: BitmapRange, source: &[u8]) {
        for (i, &src_byte) in source.iter().enumerate() {
            if let Some(dst) = self.bytes.get(range.start + i) {
                dst.fetch_or(src_byte, Ordering::Relaxed);
            }
        }
    }

    /// Reset bitmap, atomic.
    pub fn clear(&self) {
        self.bytes
            .iter()
            .for_each(|b| b.store(0, Ordering::Relaxed));
    }

    /// Atomically reads all bytes into a non-atomic `BitmapStore` snapshot.
    pub fn snapshot(&self) -> BitmapStore {
        let mut out = Vec::with_capacity(self.bytes.len());
        for b in &self.bytes {
            out.push(b.load(Ordering::Relaxed));
        }
        BitmapStore::new(out)
    }
}

/// Atomic value profiling site.
///
/// Pre-allocates `max_values_per_site` slots of (value, count). The `len`
/// field tracks how many slots are claimed. Concurrent recorders use CAS on
/// `len` to claim a new slot; existing entries are updated via fetch_add.
///
/// **Behavior change from non-atomic `ValueSite`**: when the site is full,
/// new values are dropped instead of being subject to min-count replacement.
/// Rationale: lock-free min-replacement requires (value, count) double-CAS
/// and the policy is ambiguous under concurrent fetch_add. PGO workloads
/// rarely fill sites in practice.
#[cfg(feature = "alloc")]
#[derive(Debug)]
pub(crate) struct AtomicValueSite {
    pub kind: ValueKind,
    values: Vec<AtomicU64>,
    counts: Vec<AtomicU64>,
    len: AtomicUsize,
}

#[cfg(feature = "alloc")]
impl AtomicValueSite {
    pub fn new(kind: ValueKind, capacity: usize) -> Self {
        Self {
            kind,
            values: (0..capacity).map(|_| AtomicU64::new(0)).collect(),
            counts: (0..capacity).map(|_| AtomicU64::new(0)).collect(),
            len: AtomicUsize::new(0),
        }
    }

    /// Records a (value, count) observation. Lock-free.
    pub fn record_value(&self, value: u64, count: u64) {
        if count == 0 {
            return;
        }

        // (1) Scan existing entries; if value matches, accumulate count.
        let mut scanned = self.len.load(Ordering::Acquire);
        for i in 0..scanned {
            let existing = self.values[i].load(Ordering::Relaxed);
            if existing == value {
                self.counts[i].fetch_add(count, Ordering::Relaxed);
                return;
            }
        }

        // (2) Try to claim a new slot.
        loop {
            let len = self.len.load(Ordering::Acquire);
            if len >= self.values.len() {
                return; // Full — drop new value (see struct doc).
            }
            // scanned may be stale if another thread extended concurrently;
            // re-scan new entries before claiming.
            while scanned < len {
                let existing = self.values[scanned].load(Ordering::Relaxed);
                if existing == value {
                    self.counts[scanned].fetch_add(count, Ordering::Relaxed);
                    return;
                }
                scanned += 1;
            }
            match self
                .len
                .compare_exchange(len, len + 1, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => {
                    self.values[len].store(value, Ordering::Relaxed);
                    self.counts[len].store(count, Ordering::Relaxed);
                    return;
                }
                Err(_) => {
                    core::hint::spin_loop();
                }
            }
        }
    }

    /// Clears all counts to zero. Site structure (number of slots) is preserved.
    pub fn clear_counts(&self) {
        let len = self.len.load(Ordering::Relaxed);
        for i in 0..len {
            self.counts[i].store(0, Ordering::Relaxed);
        }
    }

    /// Number of currently-occupied slots.
    pub fn len(&self) -> usize {
        self.len.load(Ordering::Relaxed)
    }

    /// Snapshot into a non-atomic `ValueSite`.
    pub fn snapshot(&self) -> ValueSite {
        let len = self.len.load(Ordering::Acquire);
        let mut values = Vec::with_capacity(len);
        for i in 0..len {
            values.push(ValueCount {
                value: self.values[i].load(Ordering::Relaxed),
                count: self.counts[i].load(Ordering::Relaxed),
            });
        }
        ValueSite {
            kind: self.kind,
            values,
        }
    }
}

/// Atomic value profiling store. Holds one `AtomicValueSite` per profiling site.
#[cfg(feature = "alloc")]
#[derive(Debug)]
pub(crate) struct AtomicValueProfileStore {
    sites: Vec<AtomicValueSite>,
    max_values_per_site: usize,
}

#[cfg(feature = "alloc")]
impl AtomicValueProfileStore {
    pub fn new(max_values_per_site: usize) -> Self {
        Self {
            sites: Vec::new(),
            max_values_per_site,
        }
    }

    /// Initializes sites from function records.
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
                    self.sites
                        .push(AtomicValueSite::new(kind, self.max_values_per_site));
                }
            }
        }
    }

    /// Records a (value, count) at the given flat site index.
    pub fn record_value(&self, site_index: usize, value: u64, count: u64) {
        let Some(site) = self.sites.get(site_index) else {
            return;
        };
        site.record_value(value, count);
    }

    /// Clears counts across all sites.
    pub fn clear_counts(&self) {
        for site in &self.sites {
            site.clear_counts();
        }
    }

    /// Total occupied value entries across all sites.
    pub fn total_value_count(&self) -> usize {
        self.sites.iter().map(|s| s.len()).sum()
    }

    /// Merges a non-atomic `ValueProfileStore` into self.
    pub fn merge_from(&self, other: &ValueProfileStore) {
        for (i, src_site) in other.sites().iter().enumerate() {
            if i >= self.sites.len() {
                break;
            }
            for vc in &src_site.values {
                self.sites[i].record_value(vc.value, vc.count);
            }
        }
    }

    /// Snapshots all sites into a non-atomic `ValueProfileStore`.
    pub fn snapshot(&self) -> ValueProfileStore {
        let mut out = ValueProfileStore::new(self.max_values_per_site);
        out.set_sites(self.sites.iter().map(|s| s.snapshot()).collect());
        out
    }
}

/// Profile format metadata.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ProfileFormat {
    pub raw_version: u64,
    pub is_byte_coverage: bool,
    pub pointer_width: usize,
}

// === Live ProfileImage (atomic fields) ===

/// Complete live profile image — built once from linker sections, mutated
/// only through `&self` atomic operations.
#[cfg(feature = "alloc")]
#[derive(Debug)]
pub(crate) struct ProfileImage {
    pub records: Vec<FunctionRecord>,
    pub counters: AtomicCounterStore,
    pub bitmap: AtomicBitmapStore,
    pub names: NameTable,
    pub value_sites: AtomicValueProfileStore,
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
            counters: self.counters.snapshot(),
            bitmap: self.bitmap.snapshot(),
            names: NameTable::new(self.names.as_slice().to_vec()),
            value_sites: self.value_sites.snapshot(),
            format: self.format,
        }
    }

    /// Reset counters, bitmap, and value counts atomically.
    pub fn reset(&self) {
        self.counters.reset();
        self.bitmap.clear();
        self.value_sites.clear_counts();
    }
}

// Helper to set sites on ValueProfileStore from atomic snapshots.
#[cfg(feature = "alloc")]
impl ValueProfileStore {
    pub(crate) fn set_sites(&mut self, sites: Vec<ValueSite>) {
        self.sites = sites;
    }
}

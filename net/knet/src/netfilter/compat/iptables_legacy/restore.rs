// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Replaces legacy filter tables and updates entry counters.

use alloc::vec::Vec;

use kerrno::{KError, KResult, LinuxError};
use klazy::Lazy;
use ksync::Mutex;

use super::{
    LegacyFilterTable, LegacyTableHandle, TableLayout,
    abi::{IptReplace, XtCounters, XtCountersInfo},
    decode::{DecodeError, LegacyTableInput},
};
use crate::netfilter::filter::{
    config::{FilterConfig, FilterConfigError, FilterGeneration},
    counter::FilterCounter,
    table::FilterTable,
};

/// A replacement table and the expected number of old entries.
pub struct LegacyFilterReplace<'a> {
    pub(super) table: LegacyTableInput<'a>,
    pub(super) num_counters: u32,
}

pub(super) static MARKER_COUNTERS: Lazy<Mutex<MarkerCounters>> =
    Lazy::new(|| Mutex::new(MarkerCounters::new()));

/// Saved counters for `ERROR` entries in one table version.
///
/// Packet matching never runs `ERROR` entries, so only `IPT_SO_SET_ADD_COUNTERS` updates them.
#[derive(Default)]
pub(super) struct MarkerCounters {
    generation: Option<FilterGeneration>,
    values: Vec<FilterCounter>,
}

impl MarkerCounters {
    pub(super) const fn new() -> Self {
        Self {
            generation: None,
            values: Vec::new(),
        }
    }

    pub(super) fn for_generation(&self, generation: FilterGeneration) -> &[FilterCounter] {
        if self.generation != Some(generation) {
            return &[];
        }
        &self.values
    }

    fn add(&mut self, generation: FilterGeneration, additions: Vec<FilterCounter>) {
        if self.generation != Some(generation) {
            self.generation = Some(generation);
            self.values = additions;
            return;
        }
        for (total, addition) in self.values.iter_mut().zip(additions) {
            total.packets = total.packets.wrapping_add(addition.packets);
            total.bytes = total.bytes.wrapping_add(addition.bytes);
        }
    }

    fn clear(&mut self) {
        self.generation = None;
        self.values.clear();
    }
}

impl IptReplace {
    /// Checks table size limits and the minimum request length, then returns the entry byte size.
    pub fn entries_size_bytes(&self, request_len_bytes: usize) -> KResult<usize> {
        if self.size as usize > super::decode::MAX_BYTES
            || self.num_entries as usize > super::decode::MAX_ENTRIES
        {
            return Err(LinuxError::E2BIG.into());
        }
        if self.num_counters == 0
            || self.num_counters as usize > super::decode::MAX_ENTRIES
            || request_len_bytes < size_of::<Self>() + self.size as usize
        {
            return Err(KError::InvalidInput);
        }
        Ok(self.size as usize)
    }

    /// Builds a replacement request from the header and entry bytes without the user counter pointer.
    pub fn replacement<'a>(&self, entries: &'a [u8]) -> LegacyFilterReplace<'a> {
        LegacyFilterReplace {
            table: LegacyTableInput {
                valid_hooks: self.valid_hooks,
                hook_entry: self.hook_entry,
                underflow: self.underflow,
                num_entries: self.num_entries,
                entries,
            },
            num_counters: self.num_counters,
        }
    }
}

impl XtCountersInfo {
    /// Checks the counter count and total request length, then returns the counter byte size.
    pub fn counters_size_bytes(&self, request_len_bytes: usize) -> KResult<usize> {
        let count = self.num_counters as usize;
        if count == 0
            || count > super::decode::MAX_ENTRIES
            || request_len_bytes != size_of::<Self>() + count * size_of::<XtCounters>()
        {
            return Err(KError::InvalidInput);
        }
        Ok(count * size_of::<XtCounters>())
    }
}

impl LegacyTableHandle<'_> {
    /// Replaces the current table after validation and returns the final old counters.
    ///
    /// If decoding fails or the old entry count does not match, the current table and counters stay unchanged.
    pub fn replace(&self, request: LegacyFilterReplace<'_>) -> KResult<Vec<XtCounters>> {
        let mut markers = self.markers.lock();
        let counters = request.replace(self.config, &markers)?;
        markers.clear();
        Ok(counters)
    }

    /// Adds packet and byte counts to each legacy entry, including `ERROR` entries.
    ///
    /// Returns `EINVAL` without changing any counter if `counters.len()` does not match the current entry count.
    pub fn add_counters(&self, counters: &[XtCounters]) -> KResult<()> {
        let mut markers = self.markers.lock();
        let mut marker_additions = Vec::new();
        let generation = self
            .config
            .add_counters(|table| {
                let (chain_additions, markers) =
                    LegacyFilterTable::split_counters(table, counters)?;
                marker_additions = markers;
                Ok(chain_additions)
            })
            .map_err(|_| KError::InvalidInput)?;
        markers.add(generation, marker_additions);
        Ok(())
    }
}

impl From<RestoreError> for KError {
    fn from(error: RestoreError) -> Self {
        match error {
            RestoreError::CounterMismatch => LinuxError::EAGAIN.into(),
            RestoreError::GenerationExhausted => LinuxError::EOVERFLOW.into(),
            RestoreError::Decode(DecodeError::TooLarge) => LinuxError::E2BIG.into(),
            RestoreError::Decode(DecodeError::Unsupported) => LinuxError::EOPNOTSUPP.into(),
            RestoreError::Decode(_) => Self::InvalidInput,
        }
    }
}

/// Reason a table replacement failed before taking effect.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RestoreError {
    Decode(DecodeError),
    CounterMismatch,
    GenerationExhausted,
}

impl LegacyFilterReplace<'_> {
    /// Decodes and validates the new table before locking, then checks the old entry count
    /// and installs the new table. Must be called in a context that can sleep.
    ///
    /// On error, the current table and counters stay unchanged. On success, the new table
    /// starts with zero counters and the old counters are returned in entry order.
    pub(super) fn replace(
        self,
        config: &FilterConfig,
        markers: &MarkerCounters,
    ) -> Result<Vec<XtCounters>, RestoreError> {
        let table = FilterTable::try_from(&self.table).map_err(RestoreError::Decode)?;
        let (old, _) = config
            .replace(table, |current, _| {
                if TableLayout::entry_count(current) != self.num_counters as usize {
                    return Err(FilterConfigError::Conflict);
                }
                Ok(())
            })
            .map_err(|error| match error {
                FilterConfigError::GenerationExhausted => RestoreError::GenerationExhausted,
                _ => RestoreError::CounterMismatch,
            })?;
        Ok(LegacyFilterTable::collect_counters(
            old.table(),
            markers.for_generation(old.generation()),
        ))
    }
}

unittest::path_mod_test!(
    tests,
    "../../../../ut-tests/netfilter/compat/iptables_legacy/restore.rs"
);

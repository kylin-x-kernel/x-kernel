// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Publishes decoded filter tables and returns final old counters.

use alloc::vec::Vec;

use super::{
    LegacyFilterTable, TableLayout,
    abi::XtCounters,
    decode::{DecodeError, LegacyTableInput},
};
use crate::netfilter::filter::{
    config::{FilterConfig, FilterConfigError},
    table::FilterTable,
};

/// A new table and the expected number of old counters.
pub(super) struct LegacyFilterReplace<'a> {
    pub(super) table: LegacyTableInput<'a>,
    pub(super) num_counters: u32,
}

/// Failure before a replacement is published.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RestoreError {
    Decode(DecodeError),
    CounterMismatch,
    GenerationExhausted,
}

impl LegacyFilterReplace<'_> {
    /// Restores and validates outside the lock, then checks the old entry count
    /// while publishing. Requires sleepable task context. Errors preserve the
    /// current table and counters; success returns final old counters in entry
    /// order, with zero counters for ERROR markers. New counters start at zero.
    pub(super) fn replace(self, config: &FilterConfig) -> Result<Vec<XtCounters>, RestoreError> {
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
        Ok(LegacyFilterTable::collect_counters(old.table()))
    }
}

unittest::path_mod_test!(
    tests,
    "../../../../ut-tests/netfilter/compat/iptables_legacy/restore.rs"
);

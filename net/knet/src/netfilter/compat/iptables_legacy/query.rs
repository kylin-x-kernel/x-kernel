// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Read the current legacy filter table.

use kerrno::{KError, KResult, LinuxError};

use super::{IptGetInfo, LegacyFilterTable, TABLE_NAME_LEN, TableLayout, abi_name};
use crate::netfilter::filter::config::{self, FilterConfig};

/// Reads the current table without holding a lock or saving a copy.
///
/// Each call reads the current table. The table may change between calls,
/// even if its size stays the same.
pub struct LegacyTableReader<'a> {
    config: &'a FilterConfig,
}

impl LegacyTableReader<'static> {
    /// Finds the global `filter` table by its ABI name.
    ///
    /// The last name byte is reserved for NUL.
    /// Returns `ENOENT` if the name is unknown.
    pub fn lookup(name: &[u8; TABLE_NAME_LEN]) -> KResult<Self> {
        if abi_name(name) != b"filter" {
            return Err(LinuxError::ENOENT.into());
        }
        Ok(Self {
            config: config::global(),
        })
    }
}

impl LegacyTableReader<'_> {
    /// Reads the current table info without encoding entries or reading counters.
    ///
    /// Must be called in a context that can sleep.
    /// Returns an error if the table is too large for the ABI fields.
    pub fn info(&self) -> KResult<IptGetInfo> {
        self.config.with_state(|state| {
            let layout = TableLayout::new(state.table()).map_err(KError::from)?;
            Ok(IptGetInfo::from(&layout))
        })
    }

    /// Exports legacy entries and reads their counters.
    ///
    /// Must be called in a context that can sleep.
    /// Returns `EAGAIN` if the table size differs from `expected_size_bytes`.
    /// The size check and export use the same table version.
    /// Errors leave the rules and counters unchanged.
    pub fn export_entries(&self, expected_size_bytes: u32) -> KResult<LegacyFilterTable> {
        self.config.with_state(|state| {
            let layout = TableLayout::new(state.table()).map_err(KError::from)?;
            if layout.size_bytes != expected_size_bytes as usize {
                return Err(LinuxError::EAGAIN.into());
            }
            LegacyFilterTable::encode(state.table(), layout).map_err(Into::into)
        })
    }
}

unittest::path_mod_test!(
    tests,
    "../../../../ut-tests/netfilter/compat/iptables_legacy/query.rs"
);

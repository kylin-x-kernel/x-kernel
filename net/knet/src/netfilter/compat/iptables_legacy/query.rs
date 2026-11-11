// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Read the current legacy filter table.

use kerrno::{KError, KResult, LinuxError};

use super::{IptGetInfo, LegacyFilterTable, LegacyTableHandle, TableLayout};

impl LegacyTableHandle<'_> {
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
        let markers = self.markers.lock();
        self.config.with_state(|state| {
            let layout = TableLayout::new(state.table()).map_err(KError::from)?;
            if layout.size_bytes != expected_size_bytes as usize {
                return Err(LinuxError::EAGAIN.into());
            }
            let marker_counters = markers.for_generation(state.generation());
            LegacyFilterTable::encode(state.table(), &layout, marker_counters).map_err(Into::into)
        })
    }
}

unittest::path_mod_test!(
    tests,
    "../../../../ut-tests/netfilter/compat/iptables_legacy/query.rs"
);

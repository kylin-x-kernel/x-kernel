// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Publication and runtime ownership for the semantic IPv4 filter table.
//!
//! Management adapters submit validated tables through this API. Wire formats,
//! entry offsets, and adapter-specific metadata remain outside the store.

use alloc::vec::Vec;

use klazy::Lazy;
use ksync::RwLock;

use super::{chain::FilterHook, counter::ChainCounters, rule::Verdict, table::FilterTable};
use crate::{buf::PacketBuf, ipv4::Ipv4Error};

static FILTER_CONFIG: Lazy<FilterConfig> =
    Lazy::new(|| FilterConfig::new(FilterTable::accept_all()));

/// Returns the global filter configuration.
pub(crate) fn global() -> &'static FilterConfig {
    &FILTER_CONFIG
}

/// Version number of a filter table.
///
/// Starts at zero and increments after each successful replacement.
/// Only comparable within the same FilterConfig instance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FilterGeneration(u64);

/// Errors when replacing a filter table or updating counters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FilterConfigError {
    Conflict,
    GenerationExhausted,
    InvalidCounterShape,
}

/// Manages the current filter table and its counters.
///
/// Requires a sleepable context. Reads and evaluations use a read lock;
/// table replacement uses a write lock.
pub(crate) struct FilterConfig {
    current: RwLock<FilterState>,
}

/// A filter table generation and its counters.
///
/// Replacement waits for all old-generation updates through `FilterConfig`.
pub(crate) struct FilterState {
    generation: FilterGeneration,
    table: FilterTable,
}

impl FilterState {
    fn new(table: FilterTable) -> Self {
        Self {
            generation: FilterGeneration(0),
            table,
        }
    }

    /// Returns the publication generation associated with this state.
    pub(crate) fn generation(&self) -> FilterGeneration {
        self.generation
    }

    /// Borrows the table and counters associated with this generation.
    pub(crate) fn table(&self) -> &FilterTable {
        &self.table
    }

    /// Returns the rule and chain counters.
    pub(crate) fn collect_counters(&self) -> Vec<ChainCounters> {
        self.table.collect_counters()
    }
}

impl FilterConfig {
    /// Creates a configuration from a validated table and its counters.
    pub(crate) fn new(table: FilterTable) -> Self {
        Self {
            current: RwLock::new(FilterState::new(table)),
        }
    }

    /// Reads the current filter state under a shared lock.
    ///
    /// Counters may change concurrently. The callback must not
    /// acquire the write lock on this configuration.
    pub(crate) fn with_state<R>(&self, read_fn: impl FnOnce(&FilterState) -> R) -> R {
        let current = self.current.read();
        read_fn(&current)
    }

    /// Publishes a validated table and returns the owned old state and new generation.
    ///
    /// The table already owns its counters before taking the publication lock.
    /// `check_fn` runs against the current table and generation under
    /// that lock; it must only inspect the table and must not update counters,
    /// perform I/O, re-enter this store, or acquire configuration/adapter locks.
    /// The generation advances under the same lock
    /// only after the check succeeds. All old-generation updates through this
    /// configuration have completed before the swap. Callers must not evaluate
    /// or update the returned old table, so its final counters can be collected
    /// outside the lock while the new generation is already in use.
    ///
    /// # Errors
    ///
    /// Returns the adapter's error, or `GenerationExhausted` if advancing the
    /// generation would overflow. Errors preserve the table, generation and counters.
    pub(crate) fn replace(
        &self,
        table: FilterTable,
        check_fn: impl FnOnce(&FilterTable, FilterGeneration) -> Result<(), FilterConfigError>,
    ) -> Result<(FilterState, FilterGeneration), FilterConfigError> {
        let mut current = self.current.write();
        check_fn(&current.table, current.generation)?;
        let generation = FilterGeneration(
            current
                .generation
                .0
                .checked_add(1)
                .ok_or(FilterConfigError::GenerationExhausted)?,
        );
        let old = core::mem::replace(&mut *current, FilterState { generation, table });
        drop(current);
        Ok((old, generation))
    }

    /// Adds counters to the generation selected while holding a shared guard.
    ///
    /// `prepare_fn` maps an adapter request to rule and chain-completion counters
    /// using the current table's rules. It follows the lock restrictions of
    /// [`Self::replace`]. Every chain shape is checked before any update applies.
    ///
    /// # Errors
    ///
    /// Returns the adapter's conflict check, or `InvalidCounterShape` if the
    /// updates do not contain exactly one counter per rule and chain completion.
    pub(crate) fn add_counters(
        &self,
        prepare_fn: impl FnOnce(&FilterTable) -> Result<Vec<ChainCounters>, FilterConfigError>,
    ) -> Result<FilterGeneration, FilterConfigError> {
        let current = self.current.read();
        let updates = prepare_fn(&current.table)?;
        current
            .table
            .add_counters(&updates)
            .map_err(|_| FilterConfigError::InvalidCounterShape)?;
        Ok(current.generation)
    }

    /// Applies INPUT to a normalized local IPv4 datagram in task context.
    pub(crate) fn accept_input(&self, packet: &PacketBuf, indev: &str) -> bool {
        self.evaluate_ipv4(FilterHook::LocalIn, packet, indev, "") == Ok(Verdict::Accept)
    }

    /// Applies OUTPUT to a finalized local IPv4 datagram before fragmentation.
    pub(crate) fn accept_output(&self, packet: &PacketBuf, outdev: &str) -> bool {
        self.evaluate_ipv4(FilterHook::LocalOut, packet, "", outdev) == Ok(Verdict::Accept)
    }

    /// Evaluates a normalized IPv4 packet against one complete generation.
    ///
    /// Matched rules and chain completions count the IPv4 length without padding.
    /// Requires the input validation or output preparation described by
    /// [`FilterTable::evaluate`]; malformed contexts return an error
    /// without changing counters.
    pub(crate) fn evaluate_ipv4(
        &self,
        hook: FilterHook,
        packet: &PacketBuf,
        indev: &str,
        outdev: &str,
    ) -> Result<Verdict, Ipv4Error> {
        let current = self.current.read();
        current.table.evaluate(hook, packet, indev, outdev)
    }
}

unittest::path_mod_test!(tests, "../../../ut-tests/netfilter/filter/config/tests.rs");

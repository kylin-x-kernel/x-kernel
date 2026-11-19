// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Native 64-bit iptables-legacy table conversion and management.

use alloc::{vec, vec::Vec};

use kerrno::{KError, KResult, LinuxError};
use ksync::Mutex;
use linux_raw_sys::net::nf_inet_hooks::{
    NF_INET_FORWARD, NF_INET_LOCAL_IN, NF_INET_LOCAL_OUT, NF_INET_NUMHOOKS,
};

use crate::netfilter::filter::{
    chain::{Chain, ChainKind, FilterHook},
    config::{self, FilterConfig, FilterConfigError},
    counter::{ChainCounters, FilterCounter},
    matcher::BaseMatch,
    rule::Target,
    table::FilterTable,
};

mod abi;
mod decode;
mod encode;
mod query;
mod restore;

use abi::{ERROR_ENTRY_SIZE, STANDARD_ENTRY_SIZE, XT_RETURN};
pub use abi::{
    EXTENSION_NAME_LEN, IptGetEntries, IptGetInfo, IptReplace, TABLE_NAME_LEN, XtCounters,
    XtCountersInfo, XtGetRevision,
};
pub(super) use encode::EncodeError;
pub use restore::LegacyFilterReplace;
use restore::{MARKER_COUNTERS, MarkerCounters};

const HOOK_COUNT: usize = NF_INET_NUMHOOKS as usize;
const FILTER_VALID_HOOKS: u32 = (1 << NF_INET_LOCAL_IN as u32)
    | (1 << NF_INET_FORWARD as u32)
    | (1 << NF_INET_LOCAL_OUT as u32);

/// Access to the current legacy filter table and its counters.
///
/// Operations require sleepable task context. The handle holds no lock or snapshot;
/// each operation accesses the current table.
pub struct LegacyTableHandle<'a> {
    config: &'a FilterConfig,
    markers: &'a Mutex<MarkerCounters>,
}

impl LegacyTableHandle<'static> {
    /// Names of the built-in IPv4 tables served by this adapter.
    pub fn registered_names() -> &'static [&'static str] {
        &["filter"]
    }

    /// Finds the global `filter` table by its ABI name.
    ///
    /// The last name byte is reserved for NUL. Returns `ENOENT` for unknown names.
    pub fn lookup(name: &[u8; TABLE_NAME_LEN]) -> KResult<Self> {
        if abi_name(name) != b"filter" {
            return Err(LinuxError::ENOENT.into());
        }
        Ok(Self {
            config: config::global(),
            markers: &MARKER_COUNTERS,
        })
    }
}

/// The extension class selected by a legacy revision query.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExtensionKind {
    Match,
    Target,
}

impl ExtensionKind {
    /// Checks a legacy record revision, reserving the final name byte for NUL.
    ///
    /// Returns `ENOENT` for unknown names and `EPROTONOSUPPORT` for unsupported
    /// revisions of a known record format. A supported revision alone does not
    /// validate a rule or table.
    pub fn check_revision(self, name: &[u8; EXTENSION_NAME_LEN], revision: u8) -> KResult<()> {
        let supported =
            encode::extension_revision(self, abi_name(name)).ok_or(LinuxError::ENOENT)?;
        if revision != supported {
            return Err(LinuxError::EPROTONOSUPPORT.into());
        }
        Ok(())
    }
}

/// Owned legacy entries and their hook and policy offsets.
#[derive(Debug, Eq, PartialEq)]
pub struct LegacyFilterTable {
    valid_hooks: u32,
    hook_entry: [u32; HOOK_COUNT],
    underflow: [u32; HOOK_COUNT],
    num_entries: u32,
    entries: Vec<u8>,
}

impl LegacyFilterTable {
    /// Encodes a validated table in chain order and samples its counters.
    ///
    /// Published tables must remain borrowed through `FilterConfig::with_state`.
    /// `ERROR` entry counters are set to zero. If encoding fails, the partial output is dropped.
    fn from_table(table: &FilterTable) -> Result<Self, EncodeError> {
        let layout = TableLayout::new(table)?;
        Self::encode(table, &layout, &[])
    }

    /// Returns the supported hook mask.
    pub(super) fn valid_hooks(&self) -> u32 {
        self.valid_hooks
    }

    /// Returns table-relative byte offsets of the built-in chain entries.
    pub(super) fn hook_entry(&self) -> &[u32; HOOK_COUNT] {
        &self.hook_entry
    }

    /// Returns table-relative byte offsets of the built-in policy entries.
    pub(super) fn underflow(&self) -> &[u32; HOOK_COUNT] {
        &self.underflow
    }

    /// Returns the entry count, including chain tails and ERROR markers.
    pub(super) fn num_entries(&self) -> u32 {
        self.num_entries
    }

    /// Returns the complete native entry array.
    pub fn entries(&self) -> &[u8] {
        &self.entries
    }

    /// Collects counters in legacy entry order, filling `ERROR` entries from `markers`.
    fn collect_counters(table: &FilterTable, markers: &[FilterCounter]) -> Vec<XtCounters> {
        let mut markers = markers.iter().copied().map(XtCounters::from);
        let mut counters = Vec::with_capacity(TableLayout::entry_count(table));
        for (chain, counts) in table.chains().iter().zip(table.collect_counters()) {
            if matches!(chain.kind(), ChainKind::UserDefined { .. }) {
                counters.push(markers.next().unwrap_or_default());
            }
            counters.extend(counts.rules.into_iter().map(XtCounters::from));
            counters.push(XtCounters::from(counts.end));
        }
        counters.push(markers.next().unwrap_or_default());
        counters
    }

    fn split_counters(
        table: &FilterTable,
        counters: &[XtCounters],
    ) -> Result<(Vec<ChainCounters>, Vec<FilterCounter>), FilterConfigError> {
        if counters.len() != TableLayout::entry_count(table) {
            return Err(FilterConfigError::InvalidCounterShape);
        }
        let mut input = counters.iter().map(|counter| FilterCounter {
            packets: counter.packets,
            bytes: counter.bytes,
        });
        let mut chains = Vec::with_capacity(table.chains().len());
        let mut markers = Vec::new();
        for chain in table.chains() {
            if matches!(chain.kind(), ChainKind::UserDefined { .. }) {
                markers.push(input.next().ok_or(FilterConfigError::InvalidCounterShape)?);
            }
            let rules = input.by_ref().take(chain.rules().len()).collect();
            let end = input.next().ok_or(FilterConfigError::InvalidCounterShape)?;
            chains.push(ChainCounters { rules, end });
        }
        markers.push(input.next().ok_or(FilterConfigError::InvalidCounterShape)?);
        Ok((chains, markers))
    }

    fn encode(
        table: &FilterTable,
        layout: &TableLayout,
        markers: &[FilterCounter],
    ) -> Result<Self, EncodeError> {
        let mut markers = markers.iter().copied();
        let mut result = Self {
            valid_hooks: FILTER_VALID_HOOKS,
            hook_entry: layout.hook_entry,
            underflow: layout.underflow,
            num_entries: layout.num_entries,
            entries: vec![0; layout.size_bytes],
        };
        // Both collections follow the same immutable table's chain and rule order.
        let counters = table.collect_counters();
        for ((chain, chain_layout), counters) in
            table.chains().iter().zip(&layout.chains).zip(counters)
        {
            let marker = matches!(chain.kind(), ChainKind::UserDefined { .. })
                .then(|| markers.next().unwrap_or_default());
            result.encode_chain(chain, chain_layout, counters, marker, layout)?;
        }
        let tail = &mut result.entries[layout.size_bytes - ERROR_ENTRY_SIZE..];
        encode::encode_error_entry(tail, "ERROR")?;
        encode::encode_counter(tail, markers.next().unwrap_or_default())?;
        Ok(result)
    }

    fn encode_chain(
        &mut self,
        chain: &Chain,
        chain_layout: &ChainLayout,
        counters: ChainCounters,
        marker: Option<FilterCounter>,
        layout: &TableLayout,
    ) -> Result<(), EncodeError> {
        if let ChainKind::UserDefined { name } = chain.kind() {
            let entry = &mut self.entries[chain_layout.head_offset..chain_layout.rules_offset];
            encode::encode_error_entry(entry, name)?;
            encode::encode_counter(entry, marker.unwrap_or_default())?;
        }

        let mut offset = chain_layout.rules_offset;
        for (rule, counter) in chain.rules().iter().zip(counters.rules) {
            let next_offset = offset + encode::entry_size(rule.extensions())?;
            let verdict = layout.resolve_target(rule.target(), next_offset)?;
            let entry = &mut self.entries[offset..next_offset];
            encode::encode_entry(entry, rule.base(), rule.extensions(), verdict)?;
            encode::encode_counter(entry, counter)?;
            offset = next_offset;
        }

        let verdict = match chain.kind() {
            ChainKind::BuiltIn { policy, .. } => encode::verdict_code(*policy),
            ChainKind::UserDefined { .. } => XT_RETURN,
        };
        let entry = &mut self.entries
            [chain_layout.tail_offset..chain_layout.tail_offset + STANDARD_ENTRY_SIZE];
        encode::encode_entry(entry, &BaseMatch::default(), &[], verdict)?;
        encode::encode_counter(entry, counters.end)
    }
}

/// Temporary layout in source-table chain order, including the terminal ERROR entry.
///
/// All byte offsets fit the signed standard-target field. The layout is used only
/// with the immutable table that produced it during one conversion.
struct TableLayout {
    chains: Vec<ChainLayout>,
    size_bytes: usize,
    num_entries: u32,
    hook_entry: [u32; HOOK_COUNT],
    underflow: [u32; HOOK_COUNT],
}

impl TableLayout {
    fn new(table: &FilterTable) -> Result<Self, EncodeError> {
        let mut chains = Vec::with_capacity(table.chains().len());
        let mut hook_entry = [0; HOOK_COUNT];
        let mut underflow = [0; HOOK_COUNT];
        let mut offset = 0usize;
        for chain in table.chains() {
            let head_offset = offset;
            let has_marker = matches!(chain.kind(), ChainKind::UserDefined { .. });
            if has_marker {
                offset = offset
                    .checked_add(ERROR_ENTRY_SIZE)
                    .ok_or(EncodeError::TableTooLarge)?;
            }
            let rules_offset = offset;
            for rule in chain.rules() {
                offset = offset
                    .checked_add(encode::entry_size(rule.extensions())?)
                    .ok_or(EncodeError::TableTooLarge)?;
            }
            let tail_offset = offset;
            offset = offset
                .checked_add(STANDARD_ENTRY_SIZE)
                .ok_or(EncodeError::TableTooLarge)?;
            chains.push(ChainLayout {
                head_offset,
                rules_offset,
                tail_offset,
            });
            if let ChainKind::BuiltIn { hook, .. } = chain.kind() {
                let hook = linux_hook(*hook);
                hook_entry[hook] =
                    u32::try_from(rules_offset).map_err(|_| EncodeError::TableTooLarge)?;
                underflow[hook] =
                    u32::try_from(tail_offset).map_err(|_| EncodeError::TableTooLarge)?;
            }
        }
        let size_bytes = offset
            .checked_add(ERROR_ENTRY_SIZE)
            .ok_or(EncodeError::TableTooLarge)?;
        // Standard jump targets are signed; this also bounds the unsigned hook offsets.
        i32::try_from(size_bytes).map_err(|_| EncodeError::TableTooLarge)?;
        let num_entries =
            u32::try_from(Self::entry_count(table)).map_err(|_| EncodeError::TableTooLarge)?;
        Ok(Self {
            chains,
            size_bytes,
            num_entries,
            hook_entry,
            underflow,
        })
    }

    /// Counts records without allocating or encoding the table.
    fn entry_count(table: &FilterTable) -> usize {
        1 + table
            .chains()
            .iter()
            .map(|chain| {
                chain.rules().len()
                    + 1
                    + usize::from(matches!(chain.kind(), ChainKind::UserDefined { .. }))
            })
            .sum::<usize>()
    }

    fn resolve_target(&self, target: Target, next_offset: usize) -> Result<i32, EncodeError> {
        let offset = match target {
            Target::Verdict(verdict) => return Ok(encode::verdict_code(verdict)),
            Target::Return => return Ok(XT_RETURN),
            Target::Continue => next_offset,
            Target::Jump(chain) => self.chains[chain.index()].rules_offset,
        };
        i32::try_from(offset).map_err(|_| EncodeError::TableTooLarge)
    }
}

/// Table-relative byte offsets; an empty chain's rules start at its tail.
struct ChainLayout {
    head_offset: usize,
    rules_offset: usize,
    tail_offset: usize,
}

/// Reserves the last byte of a nonempty ABI name field for NUL.
///
/// Returns the bytes before the first NUL in the remaining prefix.
fn abi_name<const N: usize>(name: &[u8; N]) -> &[u8] {
    let name = &name[..N - 1];
    let len = name
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(name.len());
    &name[..len]
}

impl From<EncodeError> for KError {
    fn from(error: EncodeError) -> Self {
        match error {
            EncodeError::EntryTooLarge | EncodeError::TableTooLarge => LinuxError::EOVERFLOW.into(),
            EncodeError::UnsupportedMatch => LinuxError::EOPNOTSUPP.into(),
            // The caller owns the encoder buffer; a wrong size is an internal error.
            EncodeError::BufferSize => LinuxError::EIO.into(),
            EncodeError::InvalidName => Self::InvalidInput,
        }
    }
}

fn linux_hook(hook: FilterHook) -> usize {
    match hook {
        FilterHook::LocalIn => NF_INET_LOCAL_IN as usize,
        FilterHook::Forward => NF_INET_FORWARD as usize,
        FilterHook::LocalOut => NF_INET_LOCAL_OUT as usize,
    }
}

unittest::path_mod_test!(
    tests,
    "../../../../ut-tests/netfilter/compat/iptables_legacy/tests.rs"
);

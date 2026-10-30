// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Validated IPv4 filter rules, bounded execution, and table-owned counters.

use alloc::{collections::BTreeSet, vec, vec::Vec};

use arrayvec::ArrayVec;

use super::{
    chain::{Chain, ChainId, ChainKind, FilterHook},
    counter::{ChainCounters, CounterShapeError},
    error::FilterTableError,
    packet::{MatchContext, MatchOutcome},
    rule::{Target, Verdict},
};
use crate::{buf::PacketBuf, ipv4::Ipv4Error};

// X-Kernel implementation resource limits.
pub(crate) const MAX_CHAINS: usize = 256;
pub(crate) const MAX_RULES: usize = 4096;

/// Per-packet rule-examination budget, including repeated visits to user chains.
pub(crate) const MAX_RULE_STEPS: usize = 4096;

const BUILTIN_CHAIN_COUNT: usize = FilterTable::BUILTIN_HOOKS.len();

/// Immutable, validated IPv4 `filter` rules and their mutable counters.
pub(crate) struct FilterTable {
    chains: Vec<Chain>,
    built_in_chains: [ChainId; BUILTIN_CHAIN_COUNT],
}

#[derive(Clone, Copy)]
struct JumpFrame {
    chain_id: ChainId,
    next_rule: usize,
}

impl FilterTable {
    const BUILTIN_HOOKS: [FilterHook; 3] = [
        FilterHook::LocalIn,
        FilterHook::Forward,
        FilterHook::LocalOut,
    ];

    /// Builds a table from validated chains and their counters.
    ///
    /// Requires exactly one chain per supported hook and unique chain names.
    /// Jumps must target user chains and form an acyclic graph,
    /// including chains unreachable from built-in hooks.
    pub(crate) fn new(chains: Vec<Chain>) -> Result<Self, FilterTableError> {
        if chains.len() > MAX_CHAINS {
            return Err(FilterTableError::TooManyChains);
        }

        let mut built_in_chains = [None; BUILTIN_CHAIN_COUNT];
        let mut chain_names = BTreeSet::new();
        let mut total_rules = 0usize;

        for (index, chain) in chains.iter().enumerate() {
            if chain.rules().len() > MAX_RULES - total_rules {
                return Err(FilterTableError::TooManyRules);
            }
            total_rules += chain.rules().len();
            if let ChainKind::BuiltIn { hook, .. } = chain.kind() {
                let slot = &mut built_in_chains[Self::built_in_slot(*hook)];
                if slot.is_some() {
                    return Err(FilterTableError::DuplicateHook(*hook));
                }
                *slot = Some(ChainId::from_index(index));
            }

            if !chain_names.insert(chain.name()) {
                return Err(FilterTableError::DuplicateChainName);
            }
        }

        let mut validated_chains = [ChainId::from_index(0); BUILTIN_CHAIN_COUNT];
        for (slot, hook) in Self::BUILTIN_HOOKS.into_iter().enumerate() {
            validated_chains[slot] =
                built_in_chains[slot].ok_or(FilterTableError::MissingHook(hook))?;
        }

        validate_jump_graph(&chains)?;

        Ok(Self {
            chains,
            built_in_chains: validated_chains,
        })
    }

    /// Returns an empty table whose built-in policies all accept packets.
    pub(crate) fn accept_all() -> Self {
        let chains: Vec<_> = Self::BUILTIN_HOOKS
            .map(|hook| Chain::built_in(hook, Verdict::Accept, Vec::new()))
            .into();

        Self::new(chains).expect("generated filter table must be valid")
    }

    pub(crate) fn chains(&self) -> &[Chain] {
        &self.chains
    }

    pub(crate) fn chain(&self, id: ChainId) -> Option<&Chain> {
        self.chains.get(id.index())
    }

    pub(crate) fn built_in_chain_id(&self, hook: FilterHook) -> ChainId {
        self.built_in_chains[Self::built_in_slot(hook)]
    }

    fn built_in_policy(&self, hook: FilterHook) -> Verdict {
        let chain = &self.chains[self.built_in_chain_id(hook).index()];
        // Construction maps hooks only to built-in chains; the rules are immutable.
        let ChainKind::BuiltIn { policy, .. } = chain.kind() else {
            unreachable!("filter hook must reference a built-in chain");
        };
        *policy
    }

    /// Evaluates a normalized IPv4 packet and records matched rules and chain completions.
    ///
    /// Requires IPv4 input validation or output preparation. Byte accounting
    /// uses the normalized network packet length without padding. Empty
    /// interface names represent an absent hook interface. Execution allocates
    /// no heap memory and leaves rules and packet contents unchanged.
    /// Published tables must be evaluated through [`super::config::FilterConfig`]
    /// so replacement waits for all updates to the old generation.
    ///
    /// # Errors
    ///
    /// Returns an error without updating counters if the IPv4 header or
    /// transport bounds are missing or invalidated.
    pub(crate) fn evaluate(
        &self,
        hook: FilterHook,
        packet: &PacketBuf,
        indev: &str,
        outdev: &str,
    ) -> Result<Verdict, Ipv4Error> {
        self.evaluate_with_observer(hook, packet, indev, outdev, |_, _| {})
    }

    /// Adds rule and chain-completion totals after validating the entire shape.
    ///
    /// Published tables must be updated through [`super::config::FilterConfig`]
    /// to keep the update within one publication generation.
    ///
    /// # Errors
    ///
    /// Returns `CounterShapeError` without changing counters if any chain or
    /// rule count differs from this table.
    pub(crate) fn add_counters(&self, updates: &[ChainCounters]) -> Result<(), CounterShapeError> {
        if updates.len() != self.chains.len()
            || updates
                .iter()
                .zip(&self.chains)
                .any(|(update, chain)| update.rules.len() != chain.rules().len())
        {
            return Err(CounterShapeError);
        }
        for (update, chain) in updates.iter().zip(&self.chains) {
            for (value, rule) in update.rules.iter().zip(chain.rules()) {
                rule.counter().add(value.packets, value.bytes);
            }
            chain
                .end_counter()
                .add(update.end.packets, update.end.bytes);
        }
        Ok(())
    }

    /// Collects counters in table order, allocating storage for the result.
    ///
    /// Live fields are sampled independently; packet/byte pairs and separate
    /// rules need not describe the same instant. Totals are final only after
    /// callers exclude further evaluations and batch updates.
    pub(crate) fn collect_counters(&self) -> Vec<ChainCounters> {
        self.chains
            .iter()
            .map(|chain| ChainCounters {
                rules: chain
                    .rules()
                    .iter()
                    .map(|rule| rule.counter().load())
                    .collect(),
                end: chain.end_counter().load(),
            })
            .collect()
    }

    /// Evaluates a hook and reports counter updates to an observer.
    ///
    /// Requires a packet normalized by IPv4 input validation or output preparation.
    /// Empty interface names represent an absent hook interface.
    ///
    /// After updating counters, `Some(index)` reports a matched rule before its
    /// target executes; `None` reports a chain's implicit RETURN or built-in
    /// policy. An explicit RETURN also records the policy in a built-in chain.
    /// Hot-drop and budget exhaustion drop without recording the failing rule.
    ///
    /// Execution allocates no heap memory and uses a fixed return stack bounded
    /// by the validated acyclic graph. Callbacks must suit the caller's context.
    /// Published tables follow the locking requirements of [`Self::evaluate`].
    ///
    /// Returns an error before recording counters if the IPv4 header cannot be
    /// parsed or the transport bounds are missing or invalidated.
    pub(super) fn evaluate_with_observer(
        &self,
        hook: FilterHook,
        packet: &PacketBuf,
        indev: &str,
        outdev: &str,
        mut observe_fn: impl FnMut(ChainId, Option<usize>),
    ) -> Result<Verdict, Ipv4Error> {
        let context = MatchContext::new(packet, indev, outdev)?;
        let bytes = packet.network_packet().ok_or(Ipv4Error::Malformed)?.len() as u64;
        let entry = self.built_in_chain_id(hook);
        let policy = self.built_in_policy(hook);
        let mut cursor = JumpFrame {
            chain_id: entry,
            next_rule: 0,
        };
        let mut stack = ArrayVec::<JumpFrame, MAX_CHAINS>::new();
        let mut steps = 0;

        loop {
            let chain = &self.chains()[cursor.chain_id.index()];
            if let Some(rule) = chain.rules().get(cursor.next_rule) {
                if steps == MAX_RULE_STEPS {
                    return Ok(Verdict::Drop);
                }
                steps += 1;
                match rule.evaluate_match(&context) {
                    MatchOutcome::HotDrop => return Ok(Verdict::Drop),
                    MatchOutcome::NotMatched => {
                        cursor.next_rule += 1;
                        continue;
                    }
                    MatchOutcome::Matched => {}
                }
                rule.counter().add(1, bytes);
                observe_fn(cursor.chain_id, Some(cursor.next_rule));
                cursor.next_rule += 1;
                match rule.target() {
                    Target::Continue => continue,
                    Target::Verdict(verdict) => return Ok(verdict),
                    Target::Jump(target) => {
                        // Acyclic user-chain jumps bound active frames below the chain count.
                        stack.push(cursor);
                        cursor = JumpFrame {
                            chain_id: target,
                            next_rule: 0,
                        };
                        continue;
                    }
                    Target::Return => {
                        if stack.is_empty() {
                            chain.end_counter().add(1, bytes);
                            observe_fn(cursor.chain_id, None);
                        }
                    }
                }
            } else {
                chain.end_counter().add(1, bytes);
                observe_fn(cursor.chain_id, None);
            }
            let Some(frame) = stack.pop() else {
                return Ok(policy);
            };
            cursor = frame;
        }
    }

    fn built_in_slot(hook: FilterHook) -> usize {
        match hook {
            FilterHook::LocalIn => 0,
            FilterHook::Forward => 1,
            FilterHook::LocalOut => 2,
        }
    }
}

// Kahn's algorithm checks cycles across all chains, including unreachable user chains.
fn validate_jump_graph(chains: &[Chain]) -> Result<(), FilterTableError> {
    let mut incoming_edges = vec![0usize; chains.len()];

    for chain in chains {
        for rule in chain.rules() {
            let Target::Jump(target) = rule.target() else {
                continue;
            };
            let Some(target_chain) = chains.get(target.index()) else {
                return Err(FilterTableError::InvalidJumpTarget(target));
            };
            if matches!(target_chain.kind(), ChainKind::BuiltIn { .. }) {
                return Err(FilterTableError::JumpToBuiltInChain(target));
            }
            incoming_edges[target.index()] += 1;
        }
    }

    let mut ready: Vec<_> = incoming_edges
        .iter()
        .enumerate()
        .filter_map(|(index, incoming)| (*incoming == 0).then_some(index))
        .collect();
    let mut visited = 0usize;

    while let Some(index) = ready.pop() {
        visited += 1;
        for rule in chains[index].rules() {
            let Target::Jump(target) = rule.target() else {
                continue;
            };
            incoming_edges[target.index()] -= 1;
            if incoming_edges[target.index()] == 0 {
                ready.push(target.index());
            }
        }
    }

    if visited != chains.len() {
        return Err(FilterTableError::ChainCycle);
    }

    Ok(())
}

unittest::path_mod_test!(tests, "../../../ut-tests/netfilter/filter/table/tests.rs");

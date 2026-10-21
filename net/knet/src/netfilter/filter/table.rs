// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Immutable IPv4 filter tables and structural graph validation.

use alloc::{collections::BTreeSet, vec, vec::Vec};

use super::{
    chain::{Chain, ChainId, ChainKind, FilterHook},
    error::FilterTableError,
    rule::{Target, Verdict},
};

// X-Kernel implementation resource limits.
pub(crate) const MAX_CHAINS: usize = 256;
pub(crate) const MAX_RULES: usize = 4096;

const BUILTIN_CHAIN_COUNT: usize = FilterTable::BUILTIN_HOOKS.len();

/// Immutable, validated representation of the IPv4 `filter` table.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FilterTable {
    chains: Vec<Chain>,
    built_in_chains: [ChainId; BUILTIN_CHAIN_COUNT],
}

impl FilterTable {
    const BUILTIN_HOOKS: [FilterHook; 3] = [
        FilterHook::LocalIn,
        FilterHook::Forward,
        FilterHook::LocalOut,
    ];

    /// Builds a table from validated chains.
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

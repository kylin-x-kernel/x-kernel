// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Built-in and user-defined IPv4 filter chains.

use alloc::{string::String, vec::Vec};

use super::{
    error::ChainError,
    rule::{Rule, Verdict},
};

// Linux iptables-legacy compatible chain-name limit.
// XT_EXTENSION_MAXNAMELEN = 29, including terminating NUL.
pub(crate) const MAX_CHAIN_NAME_LEN: usize = 28;

/// Netfilter hook positions supported by the IPv4 `filter` table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FilterHook {
    LocalIn,
    Forward,
    LocalOut,
}

/// Index of a chain within one [`FilterTable`](super::table::FilterTable).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ChainId(usize);

impl ChainId {
    /// Refers to the chain at `index` in the final vector passed to [`FilterTable::new`](super::table::FilterTable::new).
    ///
    /// Callers must update jump IDs when inserting, removing, or reordering chains.
    /// Table validation checks target validity; chain identity is tied to its position.
    pub(crate) const fn from_index(index: usize) -> Self {
        Self(index)
    }

    pub(crate) const fn index(self) -> usize {
        self.0
    }
}

/// Built-in or user-defined chain role.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ChainKind {
    BuiltIn { hook: FilterHook, policy: Verdict },
    UserDefined { name: String },
}

/// Ordered rules belonging to one built-in or user-defined chain.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Chain {
    kind: ChainKind,
    rules: Vec<Rule>,
}

impl Chain {
    /// Builds a built-in chain named for its hook.
    pub(crate) fn built_in(hook: FilterHook, policy: Verdict, rules: Vec<Rule>) -> Self {
        Self {
            kind: ChainKind::BuiltIn { hook, policy },
            rules,
        }
    }

    /// Builds a user chain after validating its name.
    ///
    /// # Errors
    ///
    /// Returns `EmptyName` or `NameTooLong` if the name is empty or exceeds
    /// [`MAX_CHAIN_NAME_LEN`] bytes. [`FilterTable::new`](super::table::FilterTable::new) checks uniqueness.
    pub(crate) fn user_defined(name: String, rules: Vec<Rule>) -> Result<Self, ChainError> {
        if name.is_empty() {
            return Err(ChainError::EmptyName);
        }
        if name.len() > MAX_CHAIN_NAME_LEN {
            return Err(ChainError::NameTooLong);
        }

        Ok(Self {
            kind: ChainKind::UserDefined { name },
            rules,
        })
    }

    pub(crate) fn name(&self) -> &str {
        match &self.kind {
            ChainKind::BuiltIn { hook, .. } => match hook {
                FilterHook::LocalIn => "INPUT",
                FilterHook::Forward => "FORWARD",
                FilterHook::LocalOut => "OUTPUT",
            },
            ChainKind::UserDefined { name } => name,
        }
    }

    pub(crate) const fn kind(&self) -> &ChainKind {
        &self.kind
    }

    pub(crate) fn rules(&self) -> &[Rule] {
        &self.rules
    }
}

unittest::path_mod_test!(tests, "../../../ut-tests/netfilter/filter/chain/tests.rs");

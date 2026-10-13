// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Validation errors for IPv4 filter rules, chains, and tables.

use super::chain::{ChainId, FilterHook};

/// Validation failure while constructing a filter rule.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RuleError {
    TooManyExtensionMatches,
    MissingProtocol {
        required_protocol: u8,
    },
    InvertedProtocol {
        required_protocol: u8,
        actual_protocol: u8,
    },
    ProtocolMismatch {
        required_protocol: u8,
        actual_protocol: u8,
    },
}

/// Validation failure while constructing a chain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ChainError {
    EmptyName,
    NameTooLong,
}

/// Structural validation failure for an immutable filter table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FilterTableError {
    TooManyChains,
    TooManyRules,
    DuplicateChainName,
    MissingHook(FilterHook),
    DuplicateHook(FilterHook),
    InvalidJumpTarget(ChainId),
    JumpToBuiltInChain(ChainId),
    ChainCycle,
}

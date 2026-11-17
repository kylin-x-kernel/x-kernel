// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Immutable IPv4 filter policies, publication, counters, and bounded evaluation.

pub(super) mod chain;
pub(super) mod config;
pub(super) mod counter;
mod error;
pub(super) mod matcher;
mod packet;
pub(super) mod rule;
pub(super) mod table;

unittest::path_mod_test!(tests, "../../../ut-tests/filter_evaluation.rs");

unittest::path_mod_test!(input_tests, "../../../ut-tests/filter_input.rs");

unittest::path_mod_test!(output_tests, "../../../ut-tests/filter_output.rs");

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Immutable IPv4 filter policies and bounded packet evaluation.

mod chain;
mod error;
mod matcher;
mod packet;
mod rule;
mod table;

unittest::path_mod_test!(tests, "../../../ut-tests/filter_evaluation.rs");

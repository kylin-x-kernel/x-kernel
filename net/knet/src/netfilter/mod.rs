// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Internal network-filter policies and packet evaluation.

#[expect(
    dead_code,
    reason = "CG-024-lint-exceptions: dead_code: whole-table legacy export follows in IKIXDM"
)]
mod compat;

#[expect(
    dead_code,
    reason = "CG-024-lint-exceptions: dead_code: network-path and legacy iptables consumers of \
              the private filter evaluator follow in IKIXDM"
)]
mod filter;

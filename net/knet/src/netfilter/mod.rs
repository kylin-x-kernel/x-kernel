// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Internal network-filter policy representation.

#[expect(
    dead_code,
    reason = "CG-024-lint-exceptions: dead_code: the private filter subtree is staged as one unit \
              with the follow-up rule model in IKIXDM; packet evaluation and legacy iptables \
              consumers follow"
)]
mod filter;

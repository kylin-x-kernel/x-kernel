// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Internal network-filter policies and packet evaluation.

#[cfg_attr(
    not(unittest),
    expect(
        dead_code,
        reason = "CG-024-lint-exceptions: dead_code: legacy iptables socket consumers follow in \
                  IKIXDM"
    )
)]
pub(crate) mod compat;

#[cfg_attr(
    not(unittest),
    expect(
        dead_code,
        reason = "CG-024-lint-exceptions: dead_code: remaining filter hooks and management \
                  adapters follow in IKIXDM"
    )
)]
mod filter;

pub(crate) use filter::config::{FilterConfig, global};

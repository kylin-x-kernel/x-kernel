// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Runtime scheduler knobs. [`DEFAULT_SLICE_NS`] is filled from Kconfig.

const US_TO_NS: u64 = 1_000;

const fn us_to_ns(us: usize) -> u64 {
    us as u64 * US_TO_NS
}

/// Default EEVDF request length (`SCHED_FAIR_REQUEST_US`).
pub const DEFAULT_SLICE_NS: u64 = us_to_ns(kbuild_config::SCHED_FAIR_REQUEST_US);

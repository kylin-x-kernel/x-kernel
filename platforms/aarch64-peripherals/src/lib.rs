// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! AArch64 platform peripheral drivers and helpers.
#![no_std]
#![cfg(target_arch = "aarch64")]

#[macro_use]
extern crate log;
pub mod memory;
#[cfg(any(feature = "nmi-pmu", feature = "nmi-sdei"))]
pub mod nmi;
#[cfg(feature = "pmu")]
pub mod pmu;
pub mod psci;

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! AArch64 peripheral drivers and firmware interface helpers.

#[cfg(any(feature = "kvm-guest-mem-share", feature = "kvm-mmio-guard"))]
pub mod kvm;
pub mod memory;
#[cfg(feature = "nmi")]
pub mod nmi;
#[cfg(feature = "pmu")]
pub mod pmu;
pub mod psci;
pub mod smccc;
pub mod trng;

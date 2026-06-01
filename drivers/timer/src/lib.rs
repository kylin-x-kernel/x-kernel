// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_std]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimerSource {
    PlatformStatic,
    DeviceTree,
    Acpi,
}

#[cfg(target_arch = "aarch64")]
pub mod arm_generic;
#[cfg(target_arch = "riscv64")]
pub mod riscv_sbi;
#[cfg(target_arch = "x86_64")]
pub mod x86_lapic_tsc;

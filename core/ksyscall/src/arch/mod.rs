// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#[cfg(target_arch = "riscv64")]
mod riscv_hwprobe;

#[cfg(target_arch = "riscv64")]
pub use riscv_hwprobe::*;

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_std]
#[macro_use]
extern crate log;

#[cfg(target_arch = "aarch64")]
pub mod gic;
#[cfg(target_arch = "riscv64")]
pub mod riscv;
#[cfg(target_arch = "x86_64")]
pub mod x86;

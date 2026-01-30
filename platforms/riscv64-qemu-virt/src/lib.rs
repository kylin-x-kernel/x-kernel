// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![cfg(target_arch = "riscv64")]
#![no_std]
#[macro_use]
extern crate log;
#[macro_use]
extern crate kplat;
mod boot;
mod console;
mod init;
mod irq;
mod mem;
mod power;
mod time;
pub mod config {
    platconfig_macros::include_configs!(path_env = "PLAT_CONFIG_PATH", fallback = "axconfig.toml");
}

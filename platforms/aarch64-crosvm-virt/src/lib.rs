// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform support for the aarch64 crosvm-virt target.
#![no_std]
#[macro_use]
extern crate kplat;
extern crate kernel_boot;
mod init;
mod power;
pub mod psci;

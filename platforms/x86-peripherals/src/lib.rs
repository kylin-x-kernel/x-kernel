// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! x86 platform peripheral drivers and helpers.
#![cfg(target_arch = "x86_64")]
#![no_std]
pub mod bootmem;
pub mod memory;
pub mod mp;

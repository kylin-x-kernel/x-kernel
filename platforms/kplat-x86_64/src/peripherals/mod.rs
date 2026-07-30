// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! x86 platform peripheral drivers and helpers.

#![cfg(target_arch = "x86_64")]

pub mod bootmem;
pub mod mp;

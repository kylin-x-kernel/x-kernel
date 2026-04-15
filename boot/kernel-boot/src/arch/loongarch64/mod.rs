// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

mod entry;
mod mmu;
pub(crate) mod serial;

pub const BOOT_DMW_UNCACHED_BASE: usize = 0x8000_0000_0000_0000;
pub const BOOT_DMW_BASE: usize = 0x9000_0000_0000_0000;

pub use entry::_start_secondary;

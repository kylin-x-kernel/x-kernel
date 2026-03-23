// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

pub mod linux;
pub mod multiboot;

pub use multiboot::{
    AP_START_PAGE_IDX, AP_START_PAGE_PADDR, MULTIBOOT_BOOTLOADER_MAGIC, SEV_CBIT_MASK,
};

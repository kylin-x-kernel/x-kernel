// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

pub mod linux;
pub mod multiboot;

pub use multiboot::{MULTIBOOT_BOOTLOADER_MAGIC, SEV_CBIT_MASK};

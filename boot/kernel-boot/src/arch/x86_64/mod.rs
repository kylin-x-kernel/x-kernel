// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

mod handoff;
pub mod protocols;
pub(crate) mod serial;

pub use protocols::MULTIBOOT_BOOTLOADER_MAGIC;

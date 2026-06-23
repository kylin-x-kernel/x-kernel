// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

mod el;
mod entry;
mod mmu;
pub(crate) mod serial;

pub use entry::{_start_secondary, set_secondary_boot_context};

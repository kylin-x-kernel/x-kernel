// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Trap handling.
//!
//! Re-exports the architecture exception dispatcher API so that trap
//! handlers can be registered through the HAL without depending on
//! `kcpu` directly.

pub use kcpu::excp::{IRQ, PAGE_FAULT, PageFaultFlags, register_trap_handler};

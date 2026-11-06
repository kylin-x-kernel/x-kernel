// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Execution context and interrupt-tail work, independent of IRQ mappings.
//!
//! Context tracks nesting, lifecycle supplies entry/exit observers, and softirq
//! owns pending work and its bounded runner. The private deferred bridge joins
//! the hardirq tail to that runner; it is not another public work scheduler.

pub mod context;
pub(crate) mod deferred;
pub mod lifecycle;
pub mod softirq;

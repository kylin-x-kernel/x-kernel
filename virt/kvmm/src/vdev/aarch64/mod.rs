// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! AArch64-specific virtual devices and host IRQ glue.

pub mod irq_route;
pub mod vgic;
pub mod vgicd;
pub mod vpsci;
pub mod vtimer;

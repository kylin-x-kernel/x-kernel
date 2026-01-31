// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform PSCI-related operations.

use kplat_macros::device_interface;

#[device_interface]
pub trait PsciOp {
    /// Shares a DMA buffer with the secure world (if supported).
    fn dma_share(pa: usize, size: usize);
    /// Unshares a DMA buffer from the secure world (if supported).
    fn dma_unshare(pa: usize, size: usize);
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform DMA lifecycle hooks.

use kplat_macros::device_interface;

#[device_interface]
pub trait PlatformDmaIf {
    /// Finalize a DMA mapping after page attributes have been updated.
    fn prepare(_pa: usize, _size: usize) -> kerrno::KResult {
        Ok(())
    }

    /// Tear down any platform-specific DMA state before the pages are released.
    fn release(_pa: usize, _size: usize) -> kerrno::KResult {
        Ok(())
    }
}

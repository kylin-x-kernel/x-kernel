// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform MMIO preparation hooks.

use kplat_macros::device_interface;

#[device_interface]
pub trait PlatformMmioIf {
    /// Prepare a physical MMIO range before the kernel establishes its own
    /// runtime device mapping for it.
    ///
    /// This hook runs in normal kernel context while `iomap_device()` is
    /// building a persistent mapping, not from interrupt context.
    fn prepare(_pa: usize, _size: usize) -> kerrno::KResult {
        Ok(())
    }
}

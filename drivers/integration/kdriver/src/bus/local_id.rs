// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Backend-local location ID allocator.
//!
//! Bus backends often need a small per-backend monotonically increasing
//! identifier to build [`DeviceLocation`](kdevice::DeviceLocation)
//! values (for example `FirmwareNode { id }` / `PlatformStatic { id }`).
//! Keep this helper local to `bus/` so discovery code can reuse it without
//! duplicating counters in every backend implementation.

use driver_base::{DriverError, DriverResult};

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct LocalIdAlloc {
    next: u16,
}

impl LocalIdAlloc {
    pub(crate) const fn new() -> Self {
        Self { next: 0 }
    }

    pub(crate) fn alloc(&mut self) -> DriverResult<u16> {
        let id = self.next;
        self.next = self.next.checked_add(1).ok_or(DriverError::NoMemory)?;
        Ok(id)
    }
}

unittest::path_mod_test!(local_id_unittest, "../../ut-tests/local_id_unittest.rs");

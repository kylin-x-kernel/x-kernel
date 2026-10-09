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

#[unittest::mod_test]
mod tests {
    use super::*;

    #[unittest::def_test]
    fn exhausted_local_ids_return_errors_without_wrapping() {
        let mut ids = LocalIdAlloc { next: u16::MAX - 1 };
        assert_eq!(ids.alloc(), Ok(u16::MAX - 1));
        assert_eq!(ids.alloc(), Err(DriverError::NoMemory));
        assert_eq!(ids.alloc(), Err(DriverError::NoMemory));
        assert_eq!(ids.next, u16::MAX);
    }
}

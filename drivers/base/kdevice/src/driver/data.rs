// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Type-erased storage for one driver binding's typed private data.

use alloc::boxed::Box;
use core::any::Any;

/// One binding's driver private state after type erasure.
///
/// Only the framework adapter can create it or downcast it; drivers only see
/// their associated `DeviceDriver::Data`.
pub(crate) struct DriverData {
    value: Box<dyn Any + Send>,
}

impl DriverData {
    pub(crate) fn new<T: Any + Send>(value: T) -> Self {
        Self {
            value: Box::new(value),
        }
    }

    pub(crate) fn downcast_mut<T: Any + Send>(&mut self) -> Option<&mut T> {
        self.value.downcast_mut::<T>()
    }
}

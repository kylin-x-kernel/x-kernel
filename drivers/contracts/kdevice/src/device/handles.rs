// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Stable core handles for device-model objects.

use crate::{BusId, DeviceId, DriverId};

/// Long-lived bus handle identified by a stable [`BusId`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BusHandle {
    id: BusId,
}

impl BusHandle {
    pub const fn new(id: BusId) -> Self {
        Self { id }
    }

    pub const fn id(self) -> BusId {
        self.id
    }
}

/// Long-lived driver handle identified by a stable [`DriverId`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DriverCore {
    id: DriverId,
}

impl DriverCore {
    pub const fn new(id: DriverId) -> Self {
        Self { id }
    }

    pub const fn id(self) -> DriverId {
        self.id
    }
}

/// Long-lived device handle identified by a stable [`DeviceId`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeviceCore {
    id: DeviceId,
}

impl DeviceCore {
    pub const fn new(id: DeviceId) -> Self {
        Self { id }
    }

    pub const fn id(self) -> DeviceId {
        self.id
    }
}

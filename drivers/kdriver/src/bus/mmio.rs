// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! MMIO bus probing for virtio and other devices.
#[allow(unused_imports)]
use crate::{AllDevices, prelude::*};

impl AllDevices {
    /// Probe all MMIO device ranges defined by the platform config.
    pub(crate) fn probe_bus_devices(&mut self) {
        // TODO: parse device tree to discover MMIO ranges
    }
}

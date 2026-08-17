// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `/dev/kvmm` — kvmm VM control character device.

use alloc::sync::Arc;

use kvfs::{DeviceId, DirMapping, SimpleFs};

use crate::{DeviceFile, add_device_entry};

/// Device ID for `/dev/kvmm` (misc-style major 251).
pub const KVMM_DEVICE_ID: DeviceId = DeviceId::new(251, 0);

pub(crate) fn add_root_entries(root: &mut DirMapping, fs: Arc<SimpleFs>) {
    add_device_entry(
        root,
        "kvmm",
        DeviceFile::new_character(
            fs.clone(),
            KVMM_DEVICE_ID,
            Arc::new(kvmm::KvmmDevice::new()),
        ),
    );
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `/dev/kvmm-vm` — fd-bound kvmm VM instance character device.

use alloc::sync::Arc;

use kvfs::{DeviceId, DirMapping, SimpleFs};

use crate::{DeviceFile, add_device_entry};

/// Device ID for `/dev/kvmm-vm` (misc-style major 251, minor 1).
pub(crate) const KVMM_VM_DEVICE_ID: DeviceId = DeviceId::new(251, 1);
/// Device ID for `/dev/kvm` (misc-style major 251, minor 2).
#[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
pub(crate) const KVM_DEVICE_ID: DeviceId = DeviceId::new(251, 2);

pub(crate) fn add_root_entries(root: &mut DirMapping, fs: Arc<SimpleFs>) {
    add_device_entry(
        root,
        "kvmm-vm",
        DeviceFile::new_character(
            fs.clone(),
            KVMM_VM_DEVICE_ID,
            Arc::new(kvmm_api::KvmmVmDevice::new()),
        ),
    );
    #[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
    add_device_entry(
        root,
        "kvm",
        DeviceFile::new_character(fs, KVM_DEVICE_ID, Arc::new(kvmm_api::KvmDevice::new())),
    );
}

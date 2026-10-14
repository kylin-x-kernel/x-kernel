// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::ptr::NonNull;

use unittest::def_test;
use virtio::{BufferDirection, VirtIoHal};

use super::VirtIoHalImpl;

#[def_test(serial)]
fn unshare_ignores_zero_streaming_dma_mapping() {
    let mut byte = 0u8;
    let buffer = NonNull::slice_from_raw_parts(NonNull::from(&mut byte), 1);

    // SAFETY: this mirrors the virtio-drivers cleanup path after a prior
    // share failure returned the legacy zero sentinel.
    unsafe {
        VirtIoHalImpl::unshare(0, buffer, BufferDirection::DeviceToDriver, false);
    }
}

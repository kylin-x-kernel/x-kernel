// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtIO block driver-sequence regression through the reusable device self-test.

use unittest::{assert, def_test};
use vdev_virtio_blk::selftest::vmm_selftest_virtio_blk;

/// Full virtio-blk driver sequence against the vdev-virtio-blk device: negotiation,
/// queue programming, OUT/IN/FLUSH data path, used-ring accounting and
/// interrupt injection.
#[def_test(serial)]
fn test_virtio_blk_device_driver_sequence() {
    assert!(vmm_selftest_virtio_blk());
}

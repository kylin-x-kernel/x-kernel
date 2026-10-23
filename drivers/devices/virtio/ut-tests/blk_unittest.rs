// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, def_test};

use super::*;
use crate::mock_virtio::{MockHal, MockTransport};

const VIRTIO_BLK_F_RO: u64 = 1 << 5;

#[def_test]
fn test_virtio_blk_init_failure_handling() {
    let transport = MockTransport::new();
    let dev = VirtIoBlkDev::<MockHal, MockTransport>::try_new(transport);

    if let Ok(d) = dev {
        assert!(d.name().starts_with("vd"));
        assert!(d.name().len() >= 3);
        assert_eq!(d.device_kind(), DeviceKind::Block);
        assert_eq!(d.block_size(), 512);
    } else {
        assert!(dev.is_err());
    }
}

#[def_test]
fn test_vd_name_base26() {
    assert_eq!(vd_name(0), "a");
    assert_eq!(vd_name(1), "b");
    assert_eq!(vd_name(25), "z");
    assert_eq!(vd_name(26), "aa");
    assert_eq!(vd_name(27), "ab");
    assert_eq!(vd_name(51), "az");
    assert_eq!(vd_name(52), "ba");
    assert_eq!(vd_name(701), "zz");
}

#[def_test]
fn test_virtio_blk_concurrency_traits() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<VirtIoBlkDev<MockHal, MockTransport>>();
}

#[def_test]
fn test_read_only_feature_is_reported_and_rejects_writes() {
    let mut transport = MockTransport::new();
    transport.features = VIRTIO_BLK_F_RO;
    let dev = VirtIoBlkDev::<MockHal, MockTransport>::try_new(transport)
        .expect("initialize read-only virtio block device");

    assert!(dev.is_inherently_read_only());
    assert_eq!(
        dev.write_block(0, &[0; SECTOR_SIZE]).unwrap_err(),
        DriverError::ReadOnly
    );
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, def_test};

use super::*;
use crate::mock_virtio::{MockHal, MockTransport};

#[def_test]
fn test_virtio_9p_init_failure() {
    let mut transport = MockTransport::new();
    transport.device_type = virtio_drivers::transport::DeviceType::_9P;
    let dev = VirtIo9pDev::<MockHal, MockTransport>::try_new(transport);
    assert!(dev.is_err());
}

#[def_test]
fn test_virtio_9p_mount_tag() {
    let mut transport = MockTransport::new();
    transport.device_type = virtio_drivers::transport::DeviceType::_9P;
    let tag = b"hostshare";
    let len = tag.len() as u16;
    {
        let mut config = transport.config_space.borrow_mut();
        config[0..2].copy_from_slice(&len.to_le_bytes());
        config[2..2 + tag.len()].copy_from_slice(tag);
    }

    let dev = VirtIo9pDev::<MockHal, MockTransport>::try_new(transport).unwrap();
    assert_eq!(dev.name(), "virtio-9p");
    assert_eq!(dev.device_kind(), DeviceKind::Fs9p);
    assert_eq!(dev.mount_tag(), "hostshare");
}

#[def_test]
fn test_virtio_9p_concurrency_traits() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<VirtIo9pDev<MockHal, MockTransport>>();
}

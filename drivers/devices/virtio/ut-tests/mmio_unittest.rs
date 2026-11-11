// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Mapping from raw VirtIO MMIO device IDs to driver-facing kinds.

use driver_base::DeviceKind;
use unittest::{assert_eq, def_test};

use super::device_kind_for_id;

#[def_test]
fn supported_ids_map_to_their_kind() {
    assert_eq!(device_kind_for_id(1), Some(DeviceKind::Net));
    assert_eq!(device_kind_for_id(2), Some(DeviceKind::Block));
    assert_eq!(device_kind_for_id(4), Some(DeviceKind::Hwrng));
    assert_eq!(device_kind_for_id(9), Some(DeviceKind::Fs9p));
    assert_eq!(device_kind_for_id(16), Some(DeviceKind::Display));
    assert_eq!(device_kind_for_id(18), Some(DeviceKind::Input));
    assert_eq!(device_kind_for_id(19), Some(DeviceKind::Vsock));
}

#[def_test]
fn empty_or_unsupported_ids_have_no_kind() {
    for device_id in [0, 3, 5, 13, 0xffff_ffff] {
        assert_eq!(device_kind_for_id(device_id), None);
    }
}

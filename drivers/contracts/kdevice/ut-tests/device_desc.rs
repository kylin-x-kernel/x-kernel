// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::DeviceNumber;

#[def_test]
fn device_number_round_trips_linux_components() {
    for (major, minor) in [(0, 0), (1, 2), (0x1234, 0x5678), (u32::MAX, u32::MAX)] {
        let device = DeviceNumber::new(major, minor);
        assert_eq!(device.major(), major);
        assert_eq!(device.minor(), minor);
    }
}

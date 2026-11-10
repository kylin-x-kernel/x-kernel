// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

use super::*;

#[def_test]
fn statx_reports_all_populated_basic_fields() {
    let result = statx::from(Kstat::default());
    assert_eq!(result.stx_mask & STATX_BASIC_STATS, STATX_BASIC_STATS);
}

#[def_test]
fn statx_preserves_nonzero_device_number_components() {
    let device_number = DeviceId::new(254, 0);
    let kstat = Kstat {
        dev: device_number.0,
        ..Kstat::default()
    };

    let basic_stat: stat = kstat.into();
    let extended_stat = statx::from(kstat);

    assert_eq!(basic_stat.st_dev, device_number.0);
    assert_eq!(extended_stat.stx_dev_major, device_number.major());
    assert_eq!(extended_stat.stx_dev_minor, device_number.minor());
}

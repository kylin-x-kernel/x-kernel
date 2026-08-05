// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

pub(super) fn read_platform() -> Option<ktime_types::SystemTime> {
    let unix_seconds = x86_rtc::Rtc::new().get_unix_timestamp();
    crate::system_time_from_unsigned_seconds(unix_seconds)
}

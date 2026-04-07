// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

pub fn init_platform(now_nanos: u64) {
    crate::init_unix_timestamp_offset(x86_rtc::Rtc::new().get_unix_timestamp(), now_nanos);
}

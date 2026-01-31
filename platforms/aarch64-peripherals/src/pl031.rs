// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! PL031 RTC helper for epoch offset calculation.
use arm_pl031::Rtc;
use kplat::memory::VirtAddr;

use crate::generic_timer::{now_ticks, t2ns};
static mut RTC_EPOCHOFFSET_NANOS: u64 = 0;
/// Return the cached epoch offset in nanoseconds.
#[inline]
pub fn offset_ns() -> u64 {
    unsafe { RTC_EPOCHOFFSET_NANOS }
}
/// Initialize the epoch offset using the RTC if present.
pub fn early_init(rtc_base: VirtAddr) {
    if rtc_base.as_usize() == 0 {
        return;
    }
    let rtc = unsafe { Rtc::new(rtc_base.as_mut_ptr() as _) };
    let epoch_time_nanos = rtc.get_unix_timestamp() as u64 * 1_000_000_000;
    unsafe {
        RTC_EPOCHOFFSET_NANOS = epoch_time_nanos - t2ns(now_ticks());
    }
}

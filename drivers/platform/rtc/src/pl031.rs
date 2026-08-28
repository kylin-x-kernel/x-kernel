// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use arm_pl031::Rtc;
use khal::mem::VirtAddr;

pub(super) fn read_mapped(vaddr: VirtAddr) -> Option<ktime_types::SystemTime> {
    if vaddr.as_usize() == 0 {
        return None;
    }

    // SAFETY: `vaddr` is the mapped PL031 MMIO base selected by platform init
    // and remains valid for the duration of this driver setup call.
    let rtc = unsafe { Rtc::new(vaddr.as_mut_ptr() as _) };
    crate::system_time_from_unsigned_seconds(u64::from(rtc.get_unix_timestamp()))
}

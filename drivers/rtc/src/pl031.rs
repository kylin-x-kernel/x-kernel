// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use arm_pl031::Rtc;
use kplat::memory::VirtAddr;

pub fn init_mapped(vaddr: VirtAddr, now_nanos: u64) {
    if vaddr.as_usize() == 0 {
        return;
    }

    let rtc = unsafe { Rtc::new(vaddr.as_mut_ptr() as _) };
    crate::init_unix_timestamp_offset(rtc.get_unix_timestamp() as u64, now_nanos);
}

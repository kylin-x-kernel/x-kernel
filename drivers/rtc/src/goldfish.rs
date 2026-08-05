// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use khal::mem::VirtAddr;

pub(super) fn read_mapped(vaddr: VirtAddr) -> Option<ktime_types::SystemTime> {
    if vaddr.as_usize() == 0 {
        return None;
    }

    let unix_seconds = riscv_goldfish::Rtc::new(vaddr.as_usize()).get_unix_timestamp();
    crate::system_time_from_unsigned_seconds(unix_seconds)
}

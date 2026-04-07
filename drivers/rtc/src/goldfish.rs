// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kplat::memory::VirtAddr;

pub fn init_mapped(vaddr: VirtAddr, now_nanos: u64) {
    if vaddr.as_usize() == 0 {
        return;
    }

    crate::init_unix_timestamp_offset(
        riscv_goldfish::Rtc::new(vaddr.as_usize()).get_unix_timestamp(),
        now_nanos,
    );
}

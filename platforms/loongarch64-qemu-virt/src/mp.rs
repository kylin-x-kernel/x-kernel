// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kaddr_layout::PAGE_OFFSET;
use kcpu_id_map::RawCpuId;
use khal::mem::PhysAddr;
use loongArch64::ipi::{csr_mail_send, notify_cpu_single};

const ACTION_BOOT_CPU: u32 = 1;
pub fn start_secondary_cpu(raw_cpu_id: RawCpuId, stack_top: PhysAddr) {
    let entry = kernel_boot::arch::_start_secondary as *const () as usize - PAGE_OFFSET
        + kernel_boot::arch::BOOT_DMW_BASE;
    csr_mail_send(entry as _, raw_cpu_id.as_usize(), 0);
    let stack_top = stack_top.as_usize() + kernel_boot::arch::BOOT_DMW_BASE;
    csr_mail_send(stack_top as _, raw_cpu_id.as_usize(), 1);
    notify_cpu_single(raw_cpu_id.as_usize(), ACTION_BOOT_CPU);
}

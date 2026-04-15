// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kaddr_layout::PAGE_OFFSET;
use khal::mem::PhysAddr;
use loongArch64::ipi::{csr_mail_send, notify_cpu_single};

const ACTION_BOOT_CPU: u32 = 1;
pub fn start_secondary_cpu(cpu_id: usize, stack_top: PhysAddr) {
    let entry = kernel_boot::arch::_start_secondary as *const () as usize - PAGE_OFFSET
        + kernel_boot::arch::BOOT_DMW_BASE;
    csr_mail_send(entry as _, cpu_id, 0);
    let stack_top = stack_top.as_usize() + kernel_boot::arch::BOOT_DMW_BASE;
    csr_mail_send(stack_top as _, cpu_id, 1);
    notify_cpu_single(cpu_id, ACTION_BOOT_CPU);
}

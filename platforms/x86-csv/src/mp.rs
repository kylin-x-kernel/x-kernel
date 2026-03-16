// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::time::Duration;

use kbootloader::arch::{AP_START_PAGE_IDX, AP_START_PAGE_PADDR};
use kplat::{
    memory::{PAGE_SIZE_4K, PhysAddr, pa},
    timer::spin_wait,
};

const START_PAGE_PADDR: PhysAddr = pa!(AP_START_PAGE_PADDR);

unsafe fn setup_startup_page(stack_top: PhysAddr) {
    unsafe extern "C" {
        fn ap_entry32();
        fn ap_start();
        fn ap_end();
    }
    const U64_PER_PAGE: usize = PAGE_SIZE_4K / 8;
    let start_page_ptr = kplat::memory::p2v(START_PAGE_PADDR).as_mut_ptr() as *mut u64;
    let start_page = unsafe { core::slice::from_raw_parts_mut(start_page_ptr, U64_PER_PAGE) };
    unsafe {
        core::ptr::copy_nonoverlapping(
            ap_start as *const u64,
            start_page_ptr,
            (ap_end as *const () as usize - ap_start as *const () as usize) / 8,
        );
    }
    start_page[U64_PER_PAGE - 2] = stack_top.as_usize() as u64;
    start_page[U64_PER_PAGE - 1] = ap_entry32 as *const () as usize as _;
}
pub fn start_secondary_cpu(apic_id: usize, stack_top: PhysAddr) {
    unsafe { setup_startup_page(stack_top) };
    let apic_id = x86_peripherals::apic::raw_apic_id(apic_id as u8);
    let lapic = x86_peripherals::apic::local_apic();
    unsafe { lapic.send_init_ipi(apic_id) };
    spin_wait(Duration::from_millis(10));
    unsafe { lapic.send_sipi(AP_START_PAGE_IDX, apic_id) };
    spin_wait(Duration::from_micros(200));
    unsafe { lapic.send_sipi(AP_START_PAGE_IDX, apic_id) };
}

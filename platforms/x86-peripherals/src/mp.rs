// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared x86 SMP bring-up helpers.

use core::time::Duration;

use kplat::{
    memory::{PAGE_SIZE_4K, PhysAddr, pa},
    timer::spin_wait,
};
use x86_apic as apic;

use crate::bootmem::ap_trampoline_page_paddr;

unsafe fn setup_startup_page(stack_top: PhysAddr) {
    unsafe extern "C" {
        fn ap_entry32();
        fn ap_start();
        fn ap_end();
        fn x86_ap_trampoline_protected_entry();
        static x86_ap_patch_protected_entry: u8;
        static x86_ap_patch_gdt_base: u8;
        static x86_ap_trampoline_gdt: u8;
    }

    let start_page_paddr = ap_trampoline_page_paddr();
    let start_page_ptr = kplat::memory::p2v(pa!(start_page_paddr)).as_mut_ptr();
    let start_page = unsafe { core::slice::from_raw_parts_mut(start_page_ptr, PAGE_SIZE_4K) };
    let image_base = ap_start as *const () as usize;
    let image_size = ap_end as *const () as usize - image_base;
    assert!(
        image_size <= PAGE_SIZE_4K,
        "AP trampoline must fit in a single low-memory page"
    );

    unsafe {
        core::ptr::copy_nonoverlapping(ap_start as *const u8, start_page_ptr, image_size);
    }

    let protected_entry =
        start_page_paddr + (x86_ap_trampoline_protected_entry as *const () as usize - image_base);
    let gdt_base =
        start_page_paddr + (core::ptr::addr_of!(x86_ap_trampoline_gdt) as usize - image_base);
    let protected_entry_off =
        core::ptr::addr_of!(x86_ap_patch_protected_entry) as usize - image_base;
    let gdt_base_off = core::ptr::addr_of!(x86_ap_patch_gdt_base) as usize - image_base;
    let ap_entry32_paddr = kplat::memory::v2p((ap_entry32 as *const () as usize).into()).as_usize();

    unsafe {
        start_page_ptr
            .add(protected_entry_off)
            .cast::<u32>()
            .write_unaligned(protected_entry as u32);
        start_page_ptr
            .add(gdt_base_off)
            .cast::<u32>()
            .write_unaligned(gdt_base as u32);
    }

    start_page[PAGE_SIZE_4K - 16..PAGE_SIZE_4K - 8]
        .copy_from_slice(&(stack_top.as_usize() as u64).to_le_bytes());
    start_page[PAGE_SIZE_4K - 8..PAGE_SIZE_4K]
        .copy_from_slice(&(ap_entry32_paddr as u64).to_le_bytes());
}

/// Starts a secondary CPU with the given APIC ID and stack.
pub fn start_secondary_cpu(apic_id: usize, stack_top: PhysAddr) {
    unsafe { setup_startup_page(stack_top) };

    let start_page_idx = (ap_trampoline_page_paddr() / PAGE_SIZE_4K) as u8;
    let apic_id = apic::raw_apic_id(apic_id as u8);
    let lapic = apic::local_apic();

    unsafe { lapic.send_init_ipi(apic_id) };
    spin_wait(Duration::from_millis(10));
    unsafe { lapic.send_sipi(start_page_idx, apic_id) };
    spin_wait(Duration::from_micros(200));
    unsafe { lapic.send_sipi(start_page_idx, apic_id) };
}

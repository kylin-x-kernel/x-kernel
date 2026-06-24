// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Early boot page table setup and MMU initialisation for AArch64.

use aarch64_cpu::{asm::barrier, registers::*};
use kaddr_layout::KIMAGE_VADDR;
use memaddr::PhysAddr;
use page_table::{
    PageTableEntry, PagingFlags,
    aarch64::{A64PageEntry, Arm64MemAttr},
};

use super::serial::BOOT_UART_BOOT_VADDR;
use crate::bootconsole_config;

const PT_ENTRIES: usize = 512;
const GIB: usize = 0x4000_0000;
const MIB_2: usize = 0x20_0000;
const MAX_BOOT_RAM_REGIONS: usize = 16;

#[repr(C, align(4096))]
struct PageAligned<T>(T);

impl<T: Copy> PageAligned<T> {
    const fn new(val: T) -> Self {
        Self(val)
    }
}

impl<T, const N: usize> core::ops::Index<usize> for PageAligned<[T; N]> {
    type Output = T;

    fn index(&self, idx: usize) -> &T {
        &self.0[idx]
    }
}

impl<T, const N: usize> core::ops::IndexMut<usize> for PageAligned<[T; N]> {
    fn index_mut(&mut self, idx: usize) -> &mut T {
        &mut self.0[idx]
    }
}

#[unsafe(link_section = ".data.boot_page_table")]
static mut BOOT_PT_L0_TTBR0: PageAligned<[A64PageEntry; PT_ENTRIES]> =
    PageAligned::new([A64PageEntry::EMPTY; PT_ENTRIES]);

#[unsafe(link_section = ".data.boot_page_table")]
static mut BOOT_PT_L0_TTBR1: PageAligned<[A64PageEntry; PT_ENTRIES]> =
    PageAligned::new([A64PageEntry::EMPTY; PT_ENTRIES]);

#[unsafe(link_section = ".data.boot_page_table")]
static mut BOOT_PT_L1_IDMAP: PageAligned<[A64PageEntry; PT_ENTRIES]> =
    PageAligned::new([A64PageEntry::EMPTY; PT_ENTRIES]);

#[unsafe(link_section = ".data.boot_page_table")]
static mut BOOT_PT_L1_LINEAR: PageAligned<[A64PageEntry; PT_ENTRIES]> =
    PageAligned::new([A64PageEntry::EMPTY; PT_ENTRIES]);

#[unsafe(link_section = ".data.boot_page_table")]
static mut BOOT_PT_L1_KIMAGE: PageAligned<[A64PageEntry; PT_ENTRIES]> =
    PageAligned::new([A64PageEntry::EMPTY; PT_ENTRIES]);

#[unsafe(link_section = ".data.boot_page_table")]
static mut BOOT_PT_L2_KIMAGE: PageAligned<[A64PageEntry; PT_ENTRIES]> =
    PageAligned::new([A64PageEntry::EMPTY; PT_ENTRIES]);

#[unsafe(link_section = ".data.boot_page_table")]
static mut BOOT_PT_L2_BOOT_UART: PageAligned<[A64PageEntry; PT_ENTRIES]> =
    PageAligned::new([A64PageEntry::EMPTY; PT_ENTRIES]);

macro_rules! phys_addr_of {
    ($sym:expr) => {{
        let pa: usize;
        // SAFETY: this emits a pure address-materialization sequence for a
        // linker-resolved symbol without dereferencing memory or touching the stack.
        unsafe {
            core::arch::asm!(
                "adrp {out}, {sym}",
                "add  {out}, {out}, :lo12:{sym}",
                sym = sym $sym,
                out = out(reg) pa,
                options(pure, nomem, nostack),
            );
        }
        pa
    }};
}

fn map_boot_idmap_block_if_absent(block_base: usize, flags: PagingFlags) {
    let l1_idx = block_base / GIB;
    assert!(
        l1_idx < PT_ENTRIES,
        "physical address outside boot idmap window"
    );
    // SAFETY: boot page tables are only mutated during early single-core bring-up,
    // and `l1_idx` was bounds-checked against the statically sized table.
    unsafe {
        if BOOT_PT_L1_IDMAP[l1_idx].is_present() {
            return;
        }
        BOOT_PT_L1_IDMAP[l1_idx] = A64PageEntry::new_page(PhysAddr::from(block_base), flags, true);
    }
}

fn map_boot_linear_block_if_absent(block_base: usize, flags: PagingFlags) {
    let l1_idx = block_base / GIB;
    assert!(
        l1_idx < PT_ENTRIES,
        "physical address outside boot linear map window"
    );
    // SAFETY: boot page tables are only mutated during early single-core bring-up,
    // and `l1_idx` was bounds-checked against the statically sized table.
    unsafe {
        if BOOT_PT_L1_LINEAR[l1_idx].is_present() {
            return;
        }
        BOOT_PT_L1_LINEAR[l1_idx] = A64PageEntry::new_page(PhysAddr::from(block_base), flags, true);
    }
}

unsafe fn create_boot_minimal_maps(kernel_start_pa: usize, dtb_paddr: usize) {
    if let Some(uart_paddr) = bootconsole_config::mmio_addr() {
        let uart_block_base = uart_paddr & !(GIB - 1);
        map_boot_idmap_block_if_absent(
            uart_block_base,
            PagingFlags::READ | PagingFlags::WRITE | PagingFlags::DEVICE,
        );
    }

    let kernel_block_base = kernel_start_pa & !(GIB - 1);
    map_boot_idmap_block_if_absent(
        kernel_block_base,
        PagingFlags::READ | PagingFlags::WRITE | PagingFlags::EXECUTE,
    );
    map_boot_linear_block_if_absent(
        kernel_block_base,
        PagingFlags::READ | PagingFlags::WRITE | PagingFlags::EXECUTE,
    );

    if dtb_paddr != 0 {
        let dtb_block_base = dtb_paddr & !(GIB - 1);
        map_boot_idmap_block_if_absent(
            dtb_block_base,
            PagingFlags::READ | PagingFlags::WRITE | PagingFlags::EXECUTE,
        );
        map_boot_linear_block_if_absent(
            dtb_block_base,
            PagingFlags::READ | PagingFlags::WRITE | PagingFlags::EXECUTE,
        );
    }
}

unsafe fn create_boot_kimage_map(
    kernel_start_pa: usize,
    kernel_end_pa: usize,
    l2_kimage_pa: usize,
) {
    // SAFETY: boot page tables are only mutated during early single-core bring-up,
    // and the computed index selects the dedicated L1 entry for the kernel image.
    unsafe {
        let kimage_l1_idx = (KIMAGE_VADDR & !0xFFFF_0000_0000_0000) >> 30 & (PT_ENTRIES - 1);
        BOOT_PT_L1_KIMAGE[kimage_l1_idx] = A64PageEntry::new_table(PhysAddr::from(l2_kimage_pa));
    }

    let pa_base = kernel_start_pa & !(MIB_2 - 1);
    let pa_end = (kernel_end_pa + MIB_2 - 1) & !(MIB_2 - 1);

    let mut pa = pa_base;
    let mut l2_idx = 0usize;
    while pa < pa_end {
        // SAFETY: the loop walks within the statically allocated L2 boot table
        // and fills one entry per 2 MiB chunk of the kernel image mapping.
        unsafe {
            BOOT_PT_L2_KIMAGE[l2_idx] = A64PageEntry::new_page(
                PhysAddr::from(pa),
                PagingFlags::READ | PagingFlags::WRITE | PagingFlags::EXECUTE,
                true,
            );
        }
        pa += MIB_2;
        l2_idx += 1;
    }
}

unsafe fn create_boot_uart_map(l2_boot_uart_pa: usize) {
    let Some(boot_console_paddr) = bootconsole_config::mmio_addr() else {
        return;
    };

    let boot_uart_l1_idx = (BOOT_UART_BOOT_VADDR & !0xFFFF_0000_0000_0000) >> 30 & (PT_ENTRIES - 1);
    let boot_uart_l2_idx = (BOOT_UART_BOOT_VADDR >> 21) & (PT_ENTRIES - 1);
    let boot_uart_block_pa = boot_console_paddr & !(MIB_2 - 1);
    let kimage_l1_idx = (KIMAGE_VADDR & !0xFFFF_0000_0000_0000) >> 30 & (PT_ENTRIES - 1);

    // SAFETY: boot page tables are only mutated during early single-core bring-up,
    // and the computed indices target either the dedicated UART L2 table or a
    // non-overlapping slot in the kernel-image L2 table.
    unsafe {
        if boot_uart_l1_idx == kimage_l1_idx {
            assert!(
                !BOOT_PT_L2_KIMAGE[boot_uart_l2_idx].is_present(),
                "boot UART VA overlaps kernel-image boot map"
            );
            BOOT_PT_L2_KIMAGE[boot_uart_l2_idx] = A64PageEntry::new_page(
                PhysAddr::from(boot_uart_block_pa),
                PagingFlags::READ | PagingFlags::WRITE | PagingFlags::DEVICE,
                true,
            );
        } else {
            BOOT_PT_L1_KIMAGE[boot_uart_l1_idx] =
                A64PageEntry::new_table(PhysAddr::from(l2_boot_uart_pa));
            BOOT_PT_L2_BOOT_UART[boot_uart_l2_idx] = A64PageEntry::new_page(
                PhysAddr::from(boot_uart_block_pa),
                PagingFlags::READ | PagingFlags::WRITE | PagingFlags::DEVICE,
                true,
            );
        }
    }
}

#[unsafe(link_section = ".idmap.text")]
/// # Safety
///
/// This function may only run during early single-CPU bring-up before the MMU
/// handoff completes. The caller must ensure the linker symbols, saved DTB
/// pointer, and boot page-table globals all refer to the current boot image
/// and are not concurrently mutated.
pub unsafe fn create_boot_page_tables() {
    let l1_idmap_pa = phys_addr_of!(BOOT_PT_L1_IDMAP);
    let l1_linear_pa = phys_addr_of!(BOOT_PT_L1_LINEAR);
    let l1_kimage_pa = phys_addr_of!(BOOT_PT_L1_KIMAGE);
    let l2_kimage_pa = phys_addr_of!(BOOT_PT_L2_KIMAGE);
    let l2_boot_uart_pa = phys_addr_of!(BOOT_PT_L2_BOOT_UART);

    unsafe extern "C" {
        fn _start();
        fn _ekernel();
    }
    let kernel_start_pa = phys_addr_of!(_start);
    let kernel_end_pa = phys_addr_of!(_ekernel);

    // SAFETY: boot page-table roots are single-writer globals during early
    // bring-up, and the computed indices select valid top-level slots.
    unsafe {
        BOOT_PT_L0_TTBR0[0] = A64PageEntry::new_table(PhysAddr::from(l1_idmap_pa));
        BOOT_PT_L0_TTBR1[0] = A64PageEntry::new_table(PhysAddr::from(l1_linear_pa));

        let kimage_l0_idx = (KIMAGE_VADDR & !0xFFFF_0000_0000_0000) >> 39;
        BOOT_PT_L0_TTBR1[kimage_l0_idx] = A64PageEntry::new_table(PhysAddr::from(l1_kimage_pa));
    }

    // SAFETY: early boot saved the raw DTB pointer in `SAVED_BOOT_ARGS[0]`.
    let dtb_paddr = unsafe { super::entry::SAVED_BOOT_ARGS[0] as usize };
    // SAFETY: these helpers mutate only boot-time page tables before MMU handoff.
    unsafe { create_boot_minimal_maps(kernel_start_pa, dtb_paddr) };
    // SAFETY: these helpers mutate only boot-time page tables before MMU handoff.
    unsafe { create_boot_kimage_map(kernel_start_pa, kernel_end_pa, l2_kimage_pa) };
    // SAFETY: these helpers mutate only boot-time page tables before MMU handoff.
    unsafe { create_boot_uart_map(l2_boot_uart_pa) };

    barrier::dsb(barrier::SY);
}

pub fn extend_boot_linear_ram_from_dtb(dtb_paddr: usize) {
    if dtb_paddr == 0 {
        return;
    }

    // SAFETY: `dtb_paddr` comes from boot firmware and points to the immutable
    // device-tree blob used for early RAM discovery.
    let Ok((regions, count)) = (unsafe {
        of::read_memory_regions_from_ptr::<MAX_BOOT_RAM_REGIONS>(dtb_paddr as *const u8)
    }) else {
        panic!("invalid device tree pointer: {dtb_paddr:#x}");
    };

    for region in &regions[..count] {
        if region.size == 0 {
            continue;
        }
        let start = region.starting_address as usize & !(GIB - 1);
        let end = ((region.starting_address as usize) + region.size + GIB - 1) & !(GIB - 1);
        let mut addr = start;
        while addr < end {
            map_boot_linear_block_if_absent(addr, PagingFlags::READ | PagingFlags::WRITE);
            addr += GIB;
        }
    }

    barrier::dsb(barrier::SY);
    karch::flush_tlb(None);
    barrier::isb(barrier::SY);
}

#[unsafe(link_section = ".idmap.text")]
/// # Safety
///
/// The boot page tables rooted at `BOOT_PT_L0_TTBR0` and `BOOT_PT_L0_TTBR1`
/// must already be fully initialized for the current boot image, and the
/// caller must ensure enabling the MMU will keep the current execution path
/// and required data mapped.
pub unsafe fn init_mmu() {
    let ttbr0_root_pa = phys_addr_of!(BOOT_PT_L0_TTBR0);
    let ttbr1_root_pa = phys_addr_of!(BOOT_PT_L0_TTBR1);

    MAIR_EL1.set(Arm64MemAttr::MAIR_VALUE);

    let tcr_flags0 = TCR_EL1::EPD0::EnableTTBR0Walks
        + TCR_EL1::TG0::KiB_4
        + TCR_EL1::SH0::Inner
        + TCR_EL1::ORGN0::WriteBack_ReadAlloc_WriteAlloc_Cacheable
        + TCR_EL1::IRGN0::WriteBack_ReadAlloc_WriteAlloc_Cacheable
        + TCR_EL1::T0SZ.val(16);
    let tcr_flags1 = TCR_EL1::EPD1::EnableTTBR1Walks
        + TCR_EL1::TG1::KiB_4
        + TCR_EL1::SH1::Inner
        + TCR_EL1::ORGN1::WriteBack_ReadAlloc_WriteAlloc_Cacheable
        + TCR_EL1::IRGN1::WriteBack_ReadAlloc_WriteAlloc_Cacheable
        + TCR_EL1::T1SZ.val(16);
    TCR_EL1.write(TCR_EL1::IPS::Bits_48 + TCR_EL1::AS::ASID16Bits + tcr_flags0 + tcr_flags1);
    barrier::isb(barrier::SY);

    TTBR0_EL1.set(ttbr0_root_pa as u64);
    TTBR1_EL1.set(ttbr1_root_pa as u64);

    karch::flush_tlb(None);

    SCTLR_EL1.modify(SCTLR_EL1::M::Enable + SCTLR_EL1::C::Cacheable + SCTLR_EL1::I::Cacheable);
    SCTLR_EL1.set(SCTLR_EL1.get() | (1 << 23));
    barrier::isb(barrier::SY);
    super::serial::activate_boot_map();
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Early boot page table setup and MMU initialisation for RISC-V Sv39.

use kaddr_layout::{KIMAGE_VADDR, PAGE_OFFSET};
use kbuild_config::{BOOT_CONSOLE_ADDR, BOOT_CONSOLE_TYPE};
use memaddr::{PAGE_SIZE_2M, PAGE_SIZE_4K, PhysAddr};

const PT_ENTRIES: usize = 512;
const MAX_BOOT_RAM_REGIONS: usize = 16;
const MAX_BOOT_L1_TABLES: usize = 32;

const PTE_V: u64 = 1 << 0;
const PTE_R: u64 = 1 << 1;
const PTE_W: u64 = 1 << 2;
const PTE_X: u64 = 1 << 3;
const PTE_A: u64 = 1 << 6;
const PTE_D: u64 = 1 << 7;

const PTE_TABLE: u64 = PTE_V;
const PTE_RW: u64 = PTE_V | PTE_R | PTE_W | PTE_A | PTE_D;
const PTE_RWX: u64 = PTE_RW | PTE_X;

#[derive(Clone, Copy)]
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
static mut BOOT_PT_L2: PageAligned<[u64; PT_ENTRIES]> = PageAligned::new([0; PT_ENTRIES]);

#[unsafe(link_section = ".data.boot_page_table")]
static mut BOOT_PT_L1_POOL: [PageAligned<[u64; PT_ENTRIES]>; MAX_BOOT_L1_TABLES] =
    [PageAligned::new([0; PT_ENTRIES]); MAX_BOOT_L1_TABLES];

#[unsafe(link_section = ".data.boot_page_table")]
static mut NEXT_BOOT_L1_TABLE: usize = 0;

macro_rules! phys_addr_of {
    ($sym:expr) => {{
        let pa: usize;
        unsafe {
            core::arch::asm!(
                "lla {out}, {sym}",
                sym = sym $sym,
                out = out(reg) pa,
                options(nomem, nostack),
            );
        }
        pa
    }};
}

#[inline]
const fn vpn2(va: usize) -> usize {
    (va >> 30) & 0x1ff
}

#[inline]
const fn vpn1(va: usize) -> usize {
    (va >> 21) & 0x1ff
}

#[inline]
const fn make_table_pte(paddr: usize) -> u64 {
    PTE_TABLE | (((paddr >> 12) as u64) << 10)
}

#[inline]
const fn make_leaf_pte(paddr: usize, flags: u64) -> u64 {
    flags | (((paddr >> 12) as u64) << 10)
}

#[inline]
const fn pte_paddr(pte: u64) -> usize {
    ((pte >> 10) << 12) as usize
}

#[unsafe(link_section = ".text.boot")]
unsafe fn zero_boot_tables() {
    unsafe {
        BOOT_PT_L2 = PageAligned::new([0; PT_ENTRIES]);
        BOOT_PT_L1_POOL = [PageAligned::new([0; PT_ENTRIES]); MAX_BOOT_L1_TABLES];
        NEXT_BOOT_L1_TABLE = 0;
    }
}

#[unsafe(link_section = ".text.boot")]
unsafe fn alloc_l1_table() -> (*mut PageAligned<[u64; PT_ENTRIES]>, usize) {
    let idx = unsafe { NEXT_BOOT_L1_TABLE };
    assert!(idx < MAX_BOOT_L1_TABLES, "boot L1 table pool exhausted");
    unsafe {
        NEXT_BOOT_L1_TABLE += 1;
    }

    let ptr = unsafe {
        core::ptr::addr_of_mut!(BOOT_PT_L1_POOL)
            .cast::<PageAligned<[u64; PT_ENTRIES]>>()
            .add(idx)
    };
    let pa = phys_addr_of!(BOOT_PT_L1_POOL) + idx * PAGE_SIZE_4K;
    unsafe {
        *ptr = PageAligned::new([0; PT_ENTRIES]);
    }
    (ptr, pa)
}

#[unsafe(link_section = ".text.boot")]
unsafe fn root_l1_table(root_idx: usize) -> *mut PageAligned<[u64; PT_ENTRIES]> {
    if unsafe { BOOT_PT_L2[root_idx] } == 0 {
        let (table, pa) = unsafe { alloc_l1_table() };
        unsafe {
            BOOT_PT_L2[root_idx] = make_table_pte(pa);
        }
        table
    } else {
        let pa = pte_paddr(unsafe { BOOT_PT_L2[root_idx] });
        let pool_base = phys_addr_of!(BOOT_PT_L1_POOL);
        let idx = (pa - pool_base) / PAGE_SIZE_4K;
        unsafe {
            core::ptr::addr_of_mut!(BOOT_PT_L1_POOL)
                .cast::<PageAligned<[u64; PT_ENTRIES]>>()
                .add(idx)
        }
    }
}

#[unsafe(link_section = ".text.boot")]
unsafe fn map_2m_page(va: usize, pa: usize, flags: u64) {
    let root_idx = vpn2(va);
    let l1_idx = vpn1(va);
    let table = unsafe { root_l1_table(root_idx) };
    let entry = unsafe { &mut (&mut *table)[l1_idx] };
    if *entry != 0 {
        let current_pa = pte_paddr(*entry);
        if current_pa != pa {
            panic!("conflicting boot mapping for VA {va:#x}: {current_pa:#x} != {pa:#x}");
        }
        *entry = make_leaf_pte(pa, (*entry & 0x3ff) | flags);
        return;
    }
    *entry = make_leaf_pte(pa, flags);
}

#[unsafe(link_section = ".text.boot")]
unsafe fn map_range_2m(va: usize, pa: usize, size: usize, flags: u64) {
    if size == 0 {
        return;
    }

    let va_base = va & !(PAGE_SIZE_2M - 1);
    let pa_base = pa & !(PAGE_SIZE_2M - 1);
    let end = (va + size + PAGE_SIZE_2M - 1) & !(PAGE_SIZE_2M - 1);

    let mut cur_va = va_base;
    let mut cur_pa = pa_base;
    while cur_va < end {
        unsafe { map_2m_page(cur_va, cur_pa, flags) };
        cur_va += PAGE_SIZE_2M;
        cur_pa += PAGE_SIZE_2M;
    }
}

#[unsafe(link_section = ".text.boot")]
unsafe fn map_mmio_linear_ranges() {
    if BOOT_CONSOLE_TYPE == "mmio" && BOOT_CONSOLE_ADDR != 0 {
        unsafe {
            map_range_2m(
                PAGE_OFFSET + BOOT_CONSOLE_ADDR,
                BOOT_CONSOLE_ADDR,
                PAGE_SIZE_4K,
                PTE_RW,
            )
        };
    }
}

#[unsafe(link_section = ".text.boot")]
unsafe fn map_boot_linear_ram_from_dtb(dtb_paddr: usize) {
    if dtb_paddr == 0 {
        return;
    }

    let Ok((regions, count)) = (unsafe {
        of::read_memory_regions_from_ptr::<MAX_BOOT_RAM_REGIONS>(dtb_paddr as *const u8)
    }) else {
        panic!("invalid device tree pointer: {dtb_paddr:#x}");
    };

    for region in &regions[..count] {
        if region.size == 0 {
            continue;
        }
        let start = region.starting_address as usize;
        unsafe { map_range_2m(PAGE_OFFSET + start, start, region.size, PTE_RW) };
    }
}

#[unsafe(link_section = ".text.boot")]
unsafe fn map_kernel_image(kernel_start_pa: usize, kernel_end_pa: usize) {
    let size = kernel_end_pa.saturating_sub(kernel_start_pa);
    unsafe {
        map_range_2m(kernel_start_pa, kernel_start_pa, size, PTE_RWX);
        map_range_2m(KIMAGE_VADDR, kernel_start_pa, size, PTE_RWX);
        map_range_2m(
            PAGE_OFFSET + kernel_start_pa,
            kernel_start_pa,
            size,
            PTE_RWX,
        );
    }
}

#[unsafe(link_section = ".text.boot")]
unsafe fn map_dtb_linear(dtb_paddr: usize) {
    if dtb_paddr == 0 {
        return;
    }
    unsafe { map_range_2m(PAGE_OFFSET + dtb_paddr, dtb_paddr, PAGE_SIZE_2M, PTE_RW) };
}

#[unsafe(link_section = ".text.boot")]
pub unsafe fn create_boot_page_tables(dtb_paddr: usize) {
    unsafe extern "C" {
        fn _start();
        fn _ekernel();
    }

    let kernel_start_pa = phys_addr_of!(_start);
    let kernel_end_pa = phys_addr_of!(_ekernel);

    unsafe {
        zero_boot_tables();
        map_kernel_image(kernel_start_pa, kernel_end_pa);
        map_boot_linear_ram_from_dtb(dtb_paddr);
        map_dtb_linear(dtb_paddr);
        map_mmio_linear_ranges();
    }
}

#[unsafe(link_section = ".text.boot")]
pub unsafe fn init_mmu() {
    let root_pa = phys_addr_of!(BOOT_PT_L2);
    unsafe {
        karch::write_kernel_page_table(PhysAddr::from(root_pa).into());
        karch::flush_tlb(None);
    }
}

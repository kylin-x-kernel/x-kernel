// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Early boot page table setup and MMU initialisation for LoongArch64.

use firmware_handoff::efi::{
    BootMemmapRef, ConfigurationTable, LinuxEfiBootMemmapHeader, RawEfiContext, SystemTable,
};
use kaddr_layout::{KIMAGE_VADDR, PAGE_OFFSET};
use kbuild_config::BOOT_CONSOLE_ADDR;
use loongArch64::register::tlbrentry;
use memaddr::{PAGE_SIZE_2M, PAGE_SIZE_4K, PhysAddr, pa};
use page_table::{PageTableEntry as _, PagingFlags, loongarch64::La64PageEntry as LA64PTE};

use super::{BOOT_DMW_BASE, BOOT_DMW_UNCACHED_BASE};

const PT_ENTRIES: usize = 512;
const LA64_GLOBAL_BIT: u64 = 1 << 6;
const MAX_BOOT_L1_TABLES: usize = 4;
const MAX_BOOT_L2_TABLES: usize = 8;
const MAX_BOOT_L3_TABLES: usize = 32;
const MAX_BOOT_RAM_REGIONS: usize = 32;

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

#[inline]
fn raw_entry(entry: &LA64PTE) -> u64 {
    unsafe { core::ptr::read((entry as *const LA64PTE).cast::<u64>()) }
}

#[unsafe(link_section = ".data.boot_page_table")]
static mut BOOT_KIMAGE_VOFFSET: usize = 0;
#[unsafe(link_section = ".data.boot_page_table")]
static mut BOOT_PT_ROOT: PageAligned<[LA64PTE; PT_ENTRIES]> =
    PageAligned::new([LA64PTE::EMPTY; PT_ENTRIES]);
#[unsafe(link_section = ".data.boot_page_table")]
static mut BOOT_PT_L1_POOL: [PageAligned<[LA64PTE; PT_ENTRIES]>; MAX_BOOT_L1_TABLES] =
    [PageAligned::new([LA64PTE::EMPTY; PT_ENTRIES]); MAX_BOOT_L1_TABLES];
#[unsafe(link_section = ".data.boot_page_table")]
static mut BOOT_PT_L2_POOL: [PageAligned<[LA64PTE; PT_ENTRIES]>; MAX_BOOT_L2_TABLES] =
    [PageAligned::new([LA64PTE::EMPTY; PT_ENTRIES]); MAX_BOOT_L2_TABLES];
#[unsafe(link_section = ".data.boot_page_table")]
static mut BOOT_PT_L3_POOL: [PageAligned<[LA64PTE; PT_ENTRIES]>; MAX_BOOT_L3_TABLES] =
    [PageAligned::new([LA64PTE::EMPTY; PT_ENTRIES]); MAX_BOOT_L3_TABLES];
#[unsafe(link_section = ".data.boot_page_table")]
static mut NEXT_BOOT_L1_TABLE: usize = 0;
#[unsafe(link_section = ".data.boot_page_table")]
static mut NEXT_BOOT_L2_TABLE: usize = 0;
#[unsafe(link_section = ".data.boot_page_table")]
static mut NEXT_BOOT_L3_TABLE: usize = 0;
#[unsafe(link_section = ".data.boot_page_table")]
static mut BOOT_DTB_PADDR: usize = 0;
#[unsafe(link_section = ".data.boot_page_table")]
static mut BOOT_UEFI_MEMMAP_PADDR: usize = 0;
#[unsafe(link_section = ".data.boot_page_table")]
static mut BOOT_RSDP_PADDR: usize = 0;

#[inline]
const fn root_index(va: usize) -> usize {
    (va >> 39) & 0x1ff
}

#[inline]
const fn l1_index(va: usize) -> usize {
    (va >> 30) & 0x1ff
}

#[inline]
const fn l2_index(va: usize) -> usize {
    (va >> 21) & 0x1ff
}

#[inline]
fn boot_symbol_paddr(linked_va: usize) -> PhysAddr {
    let upper = linked_va & 0xf000_0000_0000_0000;
    if upper == BOOT_DMW_BASE {
        return pa!(linked_va - BOOT_DMW_BASE);
    }
    if upper == BOOT_DMW_UNCACHED_BASE {
        return pa!(linked_va - BOOT_DMW_UNCACHED_BASE);
    }
    pa!(linked_va - unsafe { BOOT_KIMAGE_VOFFSET })
}

#[unsafe(link_section = ".text.boot")]
unsafe fn zero_boot_tables() {
    unsafe {
        BOOT_PT_ROOT = PageAligned::new([LA64PTE::EMPTY; PT_ENTRIES]);
        BOOT_PT_L1_POOL = [PageAligned::new([LA64PTE::EMPTY; PT_ENTRIES]); MAX_BOOT_L1_TABLES];
        BOOT_PT_L2_POOL = [PageAligned::new([LA64PTE::EMPTY; PT_ENTRIES]); MAX_BOOT_L2_TABLES];
        BOOT_PT_L3_POOL = [PageAligned::new([LA64PTE::EMPTY; PT_ENTRIES]); MAX_BOOT_L3_TABLES];
        NEXT_BOOT_L1_TABLE = 0;
        NEXT_BOOT_L2_TABLE = 0;
        NEXT_BOOT_L3_TABLE = 0;
    }
}

#[unsafe(link_section = ".text.boot")]
unsafe fn alloc_l1_table() -> (*mut PageAligned<[LA64PTE; PT_ENTRIES]>, PhysAddr) {
    let idx = unsafe { NEXT_BOOT_L1_TABLE };
    assert!(idx < MAX_BOOT_L1_TABLES, "boot L1 table pool exhausted");
    unsafe {
        NEXT_BOOT_L1_TABLE += 1;
    }

    let ptr = unsafe {
        core::ptr::addr_of_mut!(BOOT_PT_L1_POOL)
            .cast::<PageAligned<[LA64PTE; PT_ENTRIES]>>()
            .add(idx)
    };
    unsafe {
        *ptr = PageAligned::new([LA64PTE::EMPTY; PT_ENTRIES]);
    }
    let pa = pa!(
        boot_symbol_paddr(core::ptr::addr_of!(BOOT_PT_L1_POOL) as usize).as_usize()
            + idx * PAGE_SIZE_4K
    );
    (ptr, pa)
}

#[unsafe(link_section = ".text.boot")]
unsafe fn alloc_l2_table() -> (*mut PageAligned<[LA64PTE; PT_ENTRIES]>, PhysAddr) {
    let idx = unsafe { NEXT_BOOT_L2_TABLE };
    assert!(idx < MAX_BOOT_L2_TABLES, "boot L2 table pool exhausted");
    unsafe {
        NEXT_BOOT_L2_TABLE += 1;
    }

    let ptr = unsafe {
        core::ptr::addr_of_mut!(BOOT_PT_L2_POOL)
            .cast::<PageAligned<[LA64PTE; PT_ENTRIES]>>()
            .add(idx)
    };
    unsafe {
        *ptr = PageAligned::new([LA64PTE::EMPTY; PT_ENTRIES]);
    }
    let pa = pa!(
        boot_symbol_paddr(core::ptr::addr_of!(BOOT_PT_L2_POOL) as usize).as_usize()
            + idx * PAGE_SIZE_4K
    );
    (ptr, pa)
}

#[unsafe(link_section = ".text.boot")]
unsafe fn alloc_l3_table() -> (*mut PageAligned<[LA64PTE; PT_ENTRIES]>, PhysAddr) {
    let idx = unsafe { NEXT_BOOT_L3_TABLE };
    assert!(idx < MAX_BOOT_L3_TABLES, "boot L3 table pool exhausted");
    unsafe {
        NEXT_BOOT_L3_TABLE += 1;
    }

    let ptr = unsafe {
        core::ptr::addr_of_mut!(BOOT_PT_L3_POOL)
            .cast::<PageAligned<[LA64PTE; PT_ENTRIES]>>()
            .add(idx)
    };
    unsafe {
        *ptr = PageAligned::new([LA64PTE::EMPTY; PT_ENTRIES]);
    }
    let pa = pa!(
        boot_symbol_paddr(core::ptr::addr_of!(BOOT_PT_L3_POOL) as usize).as_usize()
            + idx * PAGE_SIZE_4K
    );
    (ptr, pa)
}

#[unsafe(link_section = ".text.boot")]
unsafe fn root_l1_table(root_idx: usize) -> *mut PageAligned<[LA64PTE; PT_ENTRIES]> {
    if unsafe { BOOT_PT_ROOT[root_idx] }.is_unused() {
        let (table, pa) = unsafe { alloc_l1_table() };
        unsafe {
            BOOT_PT_ROOT[root_idx] = LA64PTE::new_table(pa);
        }
        table
    } else {
        let pa = unsafe { BOOT_PT_ROOT[root_idx] }.paddr().as_usize();
        let pool_base = boot_symbol_paddr(core::ptr::addr_of!(BOOT_PT_L1_POOL) as usize).as_usize();
        let idx = (pa - pool_base) / PAGE_SIZE_4K;
        unsafe {
            core::ptr::addr_of_mut!(BOOT_PT_L1_POOL)
                .cast::<PageAligned<[LA64PTE; PT_ENTRIES]>>()
                .add(idx)
        }
    }
}

#[unsafe(link_section = ".text.boot")]
unsafe fn next_l2_table(
    l1_table: *mut PageAligned<[LA64PTE; PT_ENTRIES]>,
    l1_idx: usize,
) -> *mut PageAligned<[LA64PTE; PT_ENTRIES]> {
    let entry = unsafe { &mut (&mut *l1_table)[l1_idx] };
    if entry.is_unused() {
        let (table, pa) = unsafe { alloc_l2_table() };
        *entry = LA64PTE::new_table(pa);
        table
    } else {
        let pa = entry.paddr().as_usize();
        let pool_base = boot_symbol_paddr(core::ptr::addr_of!(BOOT_PT_L2_POOL) as usize).as_usize();
        let idx = (pa - pool_base) / PAGE_SIZE_4K;
        unsafe {
            core::ptr::addr_of_mut!(BOOT_PT_L2_POOL)
                .cast::<PageAligned<[LA64PTE; PT_ENTRIES]>>()
                .add(idx)
        }
    }
}

#[unsafe(link_section = ".text.boot")]
unsafe fn next_l3_table(
    l2_table: *mut PageAligned<[LA64PTE; PT_ENTRIES]>,
    l2_idx: usize,
) -> *mut PageAligned<[LA64PTE; PT_ENTRIES]> {
    let entry = unsafe { &mut (&mut *l2_table)[l2_idx] };
    if entry.is_unused() {
        let (table, pa) = unsafe { alloc_l3_table() };
        *entry = LA64PTE::new_table(pa);
        table
    } else {
        let pa = entry.paddr().as_usize();
        let pool_base = boot_symbol_paddr(core::ptr::addr_of!(BOOT_PT_L3_POOL) as usize).as_usize();
        let idx = (pa - pool_base) / PAGE_SIZE_4K;
        unsafe {
            core::ptr::addr_of_mut!(BOOT_PT_L3_POOL)
                .cast::<PageAligned<[LA64PTE; PT_ENTRIES]>>()
                .add(idx)
        }
    }
}

#[unsafe(link_section = ".text.boot")]
unsafe fn map_2m_page(va: usize, pa: usize, flags: PagingFlags) {
    let root = unsafe { root_l1_table(root_index(va)) };
    let l2 = unsafe { next_l2_table(root, l1_index(va)) };
    let entry = unsafe { &mut (&mut *l2)[l2_index(va)] };
    if entry.is_present() {
        let current_pa = entry.paddr().as_usize();
        if current_pa != pa {
            panic!("conflicting boot mapping for VA {va:#x}: {current_pa:#x} != {pa:#x}");
        }
    }
    *entry = LA64PTE::new_page(pa!(pa), flags, true);
}

#[unsafe(link_section = ".text.boot")]
unsafe fn map_4k_page(va: usize, pa: usize, flags: PagingFlags) {
    let root = unsafe { root_l1_table(root_index(va)) };
    let l2 = unsafe { next_l2_table(root, l1_index(va)) };
    let l3 = unsafe { next_l3_table(l2, l2_index(va)) };
    let entry = unsafe { &mut (&mut *l3)[(va >> 12) & 0x1ff] };
    if entry.is_present() {
        let current_pa = entry.paddr().as_usize();
        if current_pa != pa {
            panic!("conflicting boot mapping for VA {va:#x}: {current_pa:#x} != {pa:#x}");
        }
    }
    let pte = LA64PTE::new_page(pa!(pa), flags, false);
    let raw = if flags.contains(PagingFlags::USER) {
        raw_entry(&pte)
    } else {
        raw_entry(&pte) | LA64_GLOBAL_BIT
    };
    *entry = unsafe { core::mem::transmute::<u64, LA64PTE>(raw) };
}

#[unsafe(link_section = ".text.boot")]
unsafe fn map_range(va: usize, pa: usize, size: usize, flags: PagingFlags) {
    if size == 0 {
        return;
    }

    let mut cur_va = va;
    let mut cur_pa = pa;
    let end = va + size;

    while cur_va < end && ((cur_va | cur_pa) & (PAGE_SIZE_2M - 1)) != 0 {
        unsafe { map_4k_page(cur_va, cur_pa, flags) };
        cur_va += PAGE_SIZE_4K;
        cur_pa += PAGE_SIZE_4K;
    }

    while cur_va < end {
        if cur_va + PAGE_SIZE_2M <= end {
            unsafe { map_2m_page(cur_va, cur_pa, flags) };
            cur_va += PAGE_SIZE_2M;
            cur_pa += PAGE_SIZE_2M;
        } else {
            unsafe { map_4k_page(cur_va, cur_pa, flags) };
            cur_va += PAGE_SIZE_4K;
            cur_pa += PAGE_SIZE_4K;
        }
    }
}

#[inline]
const fn boot_phys_ptr<T>(paddr: usize) -> *const T {
    (BOOT_DMW_UNCACHED_BASE + paddr) as *const T
}

#[unsafe(link_section = ".text.boot")]
pub unsafe fn boot_firmware_tables() -> (usize, usize) {
    unsafe { (BOOT_DTB_PADDR, BOOT_RSDP_PADDR) }
}

#[unsafe(link_section = ".text.boot")]
pub unsafe fn boot_uefi_memmap_paddr() -> usize {
    unsafe { BOOT_UEFI_MEMMAP_PADDR }
}

#[unsafe(link_section = ".text.boot")]
unsafe fn raw_efi_context(systemtable_paddr: usize) -> RawEfiContext<'static> {
    let systemtable: *const SystemTable = boot_phys_ptr(systemtable_paddr);
    unsafe { RawEfiContext::from_ptr(systemtable) }.expect("invalid EFI system table")
}

#[unsafe(link_section = ".text.boot")]
unsafe fn raw_efi_boot_memmap(context: &RawEfiContext<'static>) -> Option<BootMemmapRef<'static>> {
    let memmap_paddr =
        unsafe { context.linux_boot_memmap_addr(boot_phys_ptr::<ConfigurationTable>) }
            .expect("invalid EFI configuration table")?;
    Some(
        unsafe { BootMemmapRef::from_ptr(boot_phys_ptr::<LinuxEfiBootMemmapHeader>(memmap_paddr)) }
            .expect("invalid Linux EFI boot memmap"),
    )
}

#[unsafe(link_section = ".text.boot")]
unsafe fn map_linear_region(phys_start: usize, size: usize, flags: PagingFlags) {
    if size == 0 {
        return;
    }
    unsafe { map_range(PAGE_OFFSET + phys_start, phys_start, size, flags) };
}

#[unsafe(link_section = ".text.boot")]
unsafe fn map_efi_metadata(context: &RawEfiContext<'static>, systemtable_paddr: usize) {
    let flags = PagingFlags::READ | PagingFlags::WRITE;
    unsafe { map_linear_region(systemtable_paddr, size_of::<SystemTable>(), flags) };
    unsafe {
        map_linear_region(
            context.configuration_table_paddr(),
            context.configuration_table_entries() * size_of::<ConfigurationTable>(),
            flags,
        )
    };
}

#[unsafe(link_section = ".text.boot")]
unsafe fn map_linear_ram_from_dtb(dtb_paddr: usize) {
    if dtb_paddr == 0 {
        return;
    }

    let Ok((regions, count)) = (unsafe {
        of::read_memory_regions_from_ptr::<MAX_BOOT_RAM_REGIONS>(boot_phys_ptr(dtb_paddr))
    }) else {
        panic!("invalid device tree pointer: {dtb_paddr:#x}");
    };

    for region in &regions[..count] {
        if region.size == 0 {
            continue;
        }
        unsafe {
            map_linear_region(
                region.starting_address as usize,
                region.size,
                PagingFlags::READ | PagingFlags::WRITE,
            )
        };
    }
}

#[unsafe(link_section = ".text.boot")]
unsafe fn map_dtb_linear(dtb_paddr: usize) {
    if dtb_paddr == 0 {
        return;
    }
    let dtb_size = unsafe { of::dtb_total_size_from_ptr(boot_phys_ptr(dtb_paddr)) }
        .unwrap_or(PAGE_SIZE_2M)
        .max(PAGE_SIZE_4K);
    unsafe { map_linear_region(dtb_paddr, dtb_size, PagingFlags::READ | PagingFlags::WRITE) };
}

#[unsafe(link_section = ".text.boot")]
unsafe fn map_linear_firmware_memory(systemtable_paddr: usize) {
    let context = unsafe { raw_efi_context(systemtable_paddr) };
    unsafe {
        BOOT_DTB_PADDR = context
            .dtb_addr(boot_phys_ptr::<ConfigurationTable>)
            .expect("invalid EFI configuration table")
            .unwrap_or(0);
        BOOT_UEFI_MEMMAP_PADDR = context
            .linux_boot_memmap_addr(boot_phys_ptr::<ConfigurationTable>)
            .expect("invalid EFI configuration table")
            .unwrap_or(0);
        BOOT_RSDP_PADDR = context
            .rsdp_addr(boot_phys_ptr::<ConfigurationTable>)
            .expect("invalid EFI configuration table")
            .unwrap_or(0);
    }
    unsafe { map_efi_metadata(&context, systemtable_paddr) };

    if let Some(memmap) = unsafe { raw_efi_boot_memmap(&context) } {
        let flags = PagingFlags::READ | PagingFlags::WRITE;
        let memmap_paddr = unsafe { BOOT_UEFI_MEMMAP_PADDR };
        assert!(
            memmap_paddr != 0,
            "missing Linux EFI boot memmap config table"
        );
        unsafe {
            map_linear_region(
                memmap_paddr,
                size_of::<LinuxEfiBootMemmapHeader>() + memmap.header().map_size,
                flags,
            )
        };
        for entry in memmap.entries() {
            if !entry.is_linear_mapping_candidate() {
                continue;
            }

            unsafe { map_linear_region(entry.phys_start(), entry.size(), flags) };
        }
        return;
    }

    let dtb_paddr = unsafe { BOOT_DTB_PADDR };
    if dtb_paddr == 0 {
        panic!("missing EFI boot memmap and device tree config table");
    }
    unsafe { map_linear_ram_from_dtb(dtb_paddr) };
    unsafe { map_dtb_linear(dtb_paddr) };
}

#[unsafe(link_section = ".text.boot")]
unsafe fn map_kernel_image(kernel_start_pa: usize) {
    unsafe extern "C" {
        fn _ekernel();
    }

    let kernel_end_pa = boot_symbol_paddr(_ekernel as *const () as usize).as_usize();
    let end_va = KIMAGE_VADDR + kernel_end_pa.saturating_sub(kernel_start_pa);
    let flags = PagingFlags::READ | PagingFlags::WRITE | PagingFlags::EXECUTE;

    unsafe {
        map_range(
            KIMAGE_VADDR,
            kernel_start_pa,
            end_va.saturating_sub(KIMAGE_VADDR),
            flags,
        )
    };
}

#[unsafe(link_section = ".text.boot")]
unsafe fn map_mmio_linear_ranges() {
    if BOOT_CONSOLE_ADDR != 0 {
        unsafe {
            map_range(
                PAGE_OFFSET + BOOT_CONSOLE_ADDR,
                BOOT_CONSOLE_ADDR,
                PAGE_SIZE_4K,
                PagingFlags::READ | PagingFlags::WRITE | PagingFlags::DEVICE,
            );
        }
    }
}

#[unsafe(link_section = ".text.boot")]
pub unsafe fn create_boot_page_tables(
    kernel_start_pa: usize,
    _cmdline_paddr: usize,
    systemtable_paddr: usize,
) {
    unsafe {
        BOOT_KIMAGE_VOFFSET = KIMAGE_VADDR - kernel_start_pa;
        zero_boot_tables();
        map_kernel_image(kernel_start_pa);
        map_linear_firmware_memory(systemtable_paddr);
        map_mmio_linear_ranges();
    }
}

#[unsafe(link_section = ".text.boot")]
pub unsafe fn init_mmu() {
    unsafe extern "C" {
        fn handle_tlb_refill();
    }

    let root = boot_symbol_paddr(core::ptr::addr_of!(BOOT_PT_ROOT) as usize);
    let tlb_refill_va = handle_tlb_refill as *const () as usize;
    let tlb_refill_pa = boot_symbol_paddr(tlb_refill_va).as_usize();
    let tlbrentry = boot_symbol_paddr(handle_tlb_refill as *const () as usize).as_usize();
    super::serial::write_str("mmu root=");
    super::serial::write_hex(root.as_usize());
    super::serial::write_str(" voffset=");
    super::serial::write_hex(unsafe { BOOT_KIMAGE_VOFFSET });
    super::serial::write_str("\n");
    super::serial::write_str("mmu tlb_refill va=");
    super::serial::write_hex(tlb_refill_va);
    super::serial::write_str(" pa=");
    super::serial::write_hex(tlb_refill_pa);
    super::serial::write_str(" tlbrentry=");
    super::serial::write_hex(tlbrentry);
    super::serial::write_str("\n");
    unsafe {
        karch::init_mmu(
            root.into(),
            pa!(0).into(),
            page_table::loongarch64::LA64MetaData::PWCL_VALUE,
            page_table::loongarch64::LA64MetaData::PWCH_VALUE,
            tlbrentry,
            0x0c,
        );
    }
    super::serial::write_str("mmu csr_tlbrentry=");
    super::serial::write_hex(tlbrentry::read().addr());
    super::serial::write_str("\n");
}

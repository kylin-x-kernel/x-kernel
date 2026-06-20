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
    // SAFETY: `BOOT_KIMAGE_VOFFSET` is initialized once during early boot
    // before any symbol translation through the relocated kernel image path.
    pa!(linked_va - unsafe { BOOT_KIMAGE_VOFFSET })
}

#[unsafe(link_section = ".text.boot")]
unsafe fn zero_boot_tables() {
    // SAFETY: boot page-table globals are single-writer state during early
    // single-CPU bring-up before MMU handoff.
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
    // SAFETY: early boot is the sole writer of the L1 table pool cursor.
    let idx = unsafe { NEXT_BOOT_L1_TABLE };
    assert!(idx < MAX_BOOT_L1_TABLES, "boot L1 table pool exhausted");
    // SAFETY: same single-writer early-boot invariant as above.
    unsafe {
        NEXT_BOOT_L1_TABLE += 1;
    }

    // SAFETY: `idx < MAX_BOOT_L1_TABLES` was checked above, so this points to
    // an in-bounds table slot in the static L1 pool.
    let ptr = unsafe {
        core::ptr::addr_of_mut!(BOOT_PT_L1_POOL)
            .cast::<PageAligned<[LA64PTE; PT_ENTRIES]>>()
            .add(idx)
    };
    // SAFETY: `ptr` names the newly allocated L1 pool slot exclusively here.
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
    // SAFETY: early boot is the sole writer of the L2 table pool cursor.
    let idx = unsafe { NEXT_BOOT_L2_TABLE };
    assert!(idx < MAX_BOOT_L2_TABLES, "boot L2 table pool exhausted");
    // SAFETY: same single-writer early-boot invariant as above.
    unsafe {
        NEXT_BOOT_L2_TABLE += 1;
    }

    // SAFETY: `idx < MAX_BOOT_L2_TABLES` was checked above, so this points to
    // an in-bounds table slot in the static L2 pool.
    let ptr = unsafe {
        core::ptr::addr_of_mut!(BOOT_PT_L2_POOL)
            .cast::<PageAligned<[LA64PTE; PT_ENTRIES]>>()
            .add(idx)
    };
    // SAFETY: `ptr` names the newly allocated L2 pool slot exclusively here.
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
    // SAFETY: early boot is the sole writer of the L3 table pool cursor.
    let idx = unsafe { NEXT_BOOT_L3_TABLE };
    assert!(idx < MAX_BOOT_L3_TABLES, "boot L3 table pool exhausted");
    // SAFETY: same single-writer early-boot invariant as above.
    unsafe {
        NEXT_BOOT_L3_TABLE += 1;
    }

    // SAFETY: `idx < MAX_BOOT_L3_TABLES` was checked above, so this points to
    // an in-bounds table slot in the static L3 pool.
    let ptr = unsafe {
        core::ptr::addr_of_mut!(BOOT_PT_L3_POOL)
            .cast::<PageAligned<[LA64PTE; PT_ENTRIES]>>()
            .add(idx)
    };
    // SAFETY: `ptr` names the newly allocated L3 pool slot exclusively here.
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
    // SAFETY: `root_idx` comes from the translated VA root index and therefore
    // stays within the root page-table bounds.
    if unsafe { BOOT_PT_ROOT[root_idx] }.is_unused() {
        // SAFETY: allocates a fresh L1 table from the boot-only pool.
        let (table, pa) = unsafe { alloc_l1_table() };
        // SAFETY: boot code is the sole mutator of the root table here.
        unsafe {
            BOOT_PT_ROOT[root_idx] = LA64PTE::new_table(pa);
        }
        table
    } else {
        // SAFETY: the root entry was initialized from the boot L1 pool, so
        // decoding its PA yields the corresponding pool slot.
        let pa = unsafe { BOOT_PT_ROOT[root_idx] }.paddr().as_usize();
        let pool_base = boot_symbol_paddr(core::ptr::addr_of!(BOOT_PT_L1_POOL) as usize).as_usize();
        let idx = (pa - pool_base) / PAGE_SIZE_4K;
        // SAFETY: `idx` was recovered from a root entry that points into the
        // static L1 pool, so this recomputes that in-bounds slot.
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
    // SAFETY: `l1_idx` is derived from the VA and indexes the selected L1 table.
    let entry = unsafe { &mut (&mut *l1_table)[l1_idx] };
    if entry.is_unused() {
        // SAFETY: allocates a fresh L2 table from the boot-only pool.
        let (table, pa) = unsafe { alloc_l2_table() };
        *entry = LA64PTE::new_table(pa);
        table
    } else {
        let pa = entry.paddr().as_usize();
        let pool_base = boot_symbol_paddr(core::ptr::addr_of!(BOOT_PT_L2_POOL) as usize).as_usize();
        let idx = (pa - pool_base) / PAGE_SIZE_4K;
        // SAFETY: `idx` was recovered from a table entry that points into the
        // static L2 pool, so this recomputes that in-bounds slot.
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
    // SAFETY: `l2_idx` is derived from the VA and indexes the selected L2 table.
    let entry = unsafe { &mut (&mut *l2_table)[l2_idx] };
    if entry.is_unused() {
        // SAFETY: allocates a fresh L3 table from the boot-only pool.
        let (table, pa) = unsafe { alloc_l3_table() };
        *entry = LA64PTE::new_table(pa);
        table
    } else {
        let pa = entry.paddr().as_usize();
        let pool_base = boot_symbol_paddr(core::ptr::addr_of!(BOOT_PT_L3_POOL) as usize).as_usize();
        let idx = (pa - pool_base) / PAGE_SIZE_4K;
        // SAFETY: `idx` was recovered from a table entry that points into the
        // static L3 pool, so this recomputes that in-bounds slot.
        unsafe {
            core::ptr::addr_of_mut!(BOOT_PT_L3_POOL)
                .cast::<PageAligned<[LA64PTE; PT_ENTRIES]>>()
                .add(idx)
        }
    }
}

#[unsafe(link_section = ".text.boot")]
unsafe fn map_2m_page(va: usize, pa: usize, flags: PagingFlags) {
    // SAFETY: the derived root index selects the boot root slot for `va`.
    let root = unsafe { root_l1_table(root_index(va)) };
    // SAFETY: the derived L1 index selects the next-level table for `va`.
    let l2 = unsafe { next_l2_table(root, l1_index(va)) };
    // SAFETY: the derived L2 index selects the target 2 MiB mapping entry.
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
    // SAFETY: the derived indices select the boot page-table path for `va`.
    let root = unsafe { root_l1_table(root_index(va)) };
    // SAFETY: see above; this walks to the boot L2 table for `va`.
    let l2 = unsafe { next_l2_table(root, l1_index(va)) };
    // SAFETY: see above; this walks to the boot L3 table for `va`.
    let l3 = unsafe { next_l3_table(l2, l2_index(va)) };
    // SAFETY: the final index selects the target 4 KiB mapping entry.
    let entry = unsafe { &mut (&mut *l3)[(va >> 12) & 0x1ff] };
    if entry.is_present() {
        let current_pa = entry.paddr().as_usize();
        if current_pa != pa {
            panic!("conflicting boot mapping for VA {va:#x}: {current_pa:#x} != {pa:#x}");
        }
    }
    let pte = LA64PTE::new_page(pa!(pa), flags, false);
    let raw = if flags.contains(PagingFlags::USER) {
        pte.as_raw()
    } else {
        pte.as_raw() | LA64_GLOBAL_BIT
    };
    *entry = LA64PTE::from_raw(raw);
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
        // SAFETY: this maps the leading unaligned 4 KiB chunk in the boot page tables.
        unsafe { map_4k_page(cur_va, cur_pa, flags) };
        cur_va += PAGE_SIZE_4K;
        cur_pa += PAGE_SIZE_4K;
    }

    while cur_va < end {
        if cur_va + PAGE_SIZE_2M <= end {
            // SAFETY: this maps a full 2 MiB chunk in the boot page tables.
            unsafe { map_2m_page(cur_va, cur_pa, flags) };
            cur_va += PAGE_SIZE_2M;
            cur_pa += PAGE_SIZE_2M;
        } else {
            // SAFETY: this maps the trailing 4 KiB chunk in the boot page tables.
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
/// # Safety
///
/// Must only be called after [`map_linear_firmware_memory`] has populated the
/// cached firmware handoff globals for the current boot image, and before any
/// later stage that may repurpose those early-boot globals.
pub unsafe fn boot_firmware_tables() -> (usize, usize) {
    // SAFETY: these globals are written once during early firmware mapping and
    // then read-only until kernel handoff.
    unsafe { (BOOT_DTB_PADDR, BOOT_RSDP_PADDR) }
}

#[unsafe(link_section = ".text.boot")]
/// # Safety
///
/// Must only be called during early boot after the EFI handoff has been
/// parsed for the current image and before those cached globals can change.
pub unsafe fn boot_uefi_memmap_paddr() -> usize {
    // SAFETY: this global is written once during early firmware mapping and
    // then read-only until kernel handoff.
    unsafe { BOOT_UEFI_MEMMAP_PADDR }
}

#[unsafe(link_section = ".text.boot")]
unsafe fn raw_efi_context(systemtable_paddr: usize) -> RawEfiContext<'static> {
    let systemtable: *const SystemTable = boot_phys_ptr(systemtable_paddr);
    // SAFETY: `systemtable_paddr` comes from firmware handoff and points to a
    // readable EFI system table in boot-physical addressing.
    unsafe { RawEfiContext::from_ptr(systemtable) }.expect("invalid EFI system table")
}

#[unsafe(link_section = ".text.boot")]
unsafe fn raw_efi_boot_memmap(context: &RawEfiContext<'static>) -> Option<BootMemmapRef<'static>> {
    let memmap_paddr =
        // SAFETY: `context` was constructed from the live EFI system table and
        // the configuration-table pointer remains readable during early boot.
        unsafe { context.linux_boot_memmap_addr(boot_phys_ptr::<ConfigurationTable>) }
            .expect("invalid EFI configuration table")?;
    Some(
        // SAFETY: `memmap_paddr` comes from the validated Linux EFI memmap
        // config-table entry and points to a readable boot memmap header.
        unsafe { BootMemmapRef::from_ptr(boot_phys_ptr::<LinuxEfiBootMemmapHeader>(memmap_paddr)) }
            .expect("invalid Linux EFI boot memmap"),
    )
}

#[unsafe(link_section = ".text.boot")]
unsafe fn map_linear_region(phys_start: usize, size: usize, flags: PagingFlags) {
    if size == 0 {
        return;
    }
    // SAFETY: this extends the boot linear map over the caller-provided physical range.
    unsafe { map_range(PAGE_OFFSET + phys_start, phys_start, size, flags) };
}

#[unsafe(link_section = ".text.boot")]
unsafe fn map_efi_metadata(context: &RawEfiContext<'static>, systemtable_paddr: usize) {
    let flags = PagingFlags::READ | PagingFlags::WRITE;
    // SAFETY: the EFI system table is firmware-owned metadata that must remain linearly mapped.
    unsafe { map_linear_region(systemtable_paddr, size_of::<SystemTable>(), flags) };
    // SAFETY: the EFI configuration table array is firmware-owned metadata that
    // must remain linearly mapped during early boot parsing.
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

    // SAFETY: `dtb_paddr` is the firmware-provided DTB pointer and this helper
    // only reads its memory-region description.
    let Ok((regions, count)) = (unsafe {
        of::read_memory_regions_from_ptr::<MAX_BOOT_RAM_REGIONS>(boot_phys_ptr(dtb_paddr))
    }) else {
        panic!("invalid device tree pointer: {dtb_paddr:#x}");
    };

    for region in &regions[..count] {
        if region.size == 0 {
            continue;
        }
        // SAFETY: each region comes from the validated DTB memory map and is
        // added only to the boot linear mapping.
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
    // SAFETY: `dtb_paddr` is the firmware-provided DTB pointer; this only reads
    // the DTB header to determine the total mapped size.
    let dtb_size = unsafe { of::dtb_total_size_from_ptr(boot_phys_ptr(dtb_paddr)) }
        .unwrap_or(PAGE_SIZE_2M)
        .max(PAGE_SIZE_4K);
    // SAFETY: the DTB is immutable firmware data that must remain reachable through the linear map.
    unsafe { map_linear_region(dtb_paddr, dtb_size, PagingFlags::READ | PagingFlags::WRITE) };
}

#[unsafe(link_section = ".text.boot")]
unsafe fn map_linear_firmware_memory(systemtable_paddr: usize) {
    // SAFETY: `systemtable_paddr` comes from firmware handoff and points to the
    // live EFI system table for this boot.
    let context = unsafe { raw_efi_context(systemtable_paddr) };
    // SAFETY: early boot performs one-time caching of firmware table addresses
    // into boot globals before kernel handoff.
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
    // SAFETY: the validated EFI metadata described by `context` must be linearly mapped.
    unsafe { map_efi_metadata(&context, systemtable_paddr) };

    // SAFETY: the validated EFI context may expose a Linux EFI boot memmap config table.
    if let Some(memmap) = unsafe { raw_efi_boot_memmap(&context) } {
        let flags = PagingFlags::READ | PagingFlags::WRITE;
        // SAFETY: boot globals were initialized just above and remain stable here.
        let memmap_paddr = unsafe { BOOT_UEFI_MEMMAP_PADDR };
        assert!(
            memmap_paddr != 0,
            "missing Linux EFI boot memmap config table"
        );
        // SAFETY: this maps the EFI boot memmap header and entry array described by firmware.
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

            // SAFETY: firmware marked this region as a linear-mapping candidate.
            unsafe { map_linear_region(entry.phys_start(), entry.size(), flags) };
        }
        return;
    }

    // SAFETY: boot globals were initialized above and remain stable here.
    let dtb_paddr = unsafe { BOOT_DTB_PADDR };
    if dtb_paddr == 0 {
        panic!("missing EFI boot memmap and device tree config table");
    }
    // SAFETY: falls back to the DTB-described RAM map for linear mapping.
    unsafe { map_linear_ram_from_dtb(dtb_paddr) };
    // SAFETY: ensures the DTB blob itself remains linearly mapped.
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

    // SAFETY: this maps the kernel image range into the boot page tables during
    // one-time early bring-up.
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
        // SAFETY: this installs the boot console MMIO window into the boot linear map.
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
/// # Safety
///
/// This function may only run during early single-CPU bring-up before the
/// final runtime MMU state is installed. The caller must pass the physical
/// kernel start address for the current image plus the live firmware handoff
/// pointers for this boot.
pub unsafe fn create_boot_page_tables(
    kernel_start_pa: usize,
    _cmdline_paddr: usize,
    systemtable_paddr: usize,
) {
    // SAFETY: this is the one-time early-boot page-table construction phase
    // before secondary CPUs start or MMU handoff completes.
    unsafe {
        BOOT_KIMAGE_VOFFSET = KIMAGE_VADDR - kernel_start_pa;
        zero_boot_tables();
        map_kernel_image(kernel_start_pa);
        map_linear_firmware_memory(systemtable_paddr);
        map_mmio_linear_ranges();
    }
}

#[unsafe(link_section = ".text.boot")]
/// # Safety
///
/// `BOOT_PT_ROOT` and the associated LoongArch boot paging metadata must
/// already be fully initialized for the current image, and enabling the MMU
/// must preserve the current execution path and required boot data mappings.
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
    // SAFETY: `BOOT_KIMAGE_VOFFSET` was initialized during boot page-table construction
    // before `init_mmu` is reached.
    super::serial::write_hex(unsafe { BOOT_KIMAGE_VOFFSET });
    super::serial::write_str("\n");
    super::serial::write_str("mmu tlb_refill va=");
    super::serial::write_hex(tlb_refill_va);
    super::serial::write_str(" pa=");
    super::serial::write_hex(tlb_refill_pa);
    super::serial::write_str(" tlbrentry=");
    super::serial::write_hex(tlbrentry);
    super::serial::write_str("\n");
    // SAFETY: boot page tables and MMU metadata were fully initialized by
    // `create_boot_page_tables`, and this programs the current CPU's MMU state.
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

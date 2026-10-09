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
        // SAFETY: this emits a pure address-materialization sequence for a
        // linker-visible boot symbol without touching memory or the stack.
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

/// # Safety
///
/// Call only during single-CPU boot, with exclusive access to the boot
/// tables and pool cursors, before hardware page walks use these tables.
/// No outstanding references may access any table being reset, and no
/// previously returned pool pointer may be used after this reset.
#[unsafe(link_section = ".text.boot")]
unsafe fn zero_boot_tables() {
    // SAFETY: Single-CPU boot exclusively owns BOOT_PT_L2; no hardware
    // walks or outstanding pool references exist during this reset.
    unsafe { BOOT_PT_L2 = PageAligned::new([0; PT_ENTRIES]) };
    // SAFETY: Single-CPU boot exclusively owns BOOT_PT_L1_POOL; no hardware
    // walks or outstanding pool references exist during this reset.
    unsafe { BOOT_PT_L1_POOL = [PageAligned::new([0; PT_ENTRIES]); MAX_BOOT_L1_TABLES] };
    // SAFETY: Single-CPU boot exclusively owns NEXT_BOOT_L1_TABLE; no hardware
    // walks or outstanding pool references exist during this reset.
    unsafe { NEXT_BOOT_L1_TABLE = 0 };
}

/// # Safety
///
/// Call only during single-CPU boot, with exclusive access to the boot
/// tables and pool cursors, before hardware page walks use these tables.
/// The L1 pool and cursor must have been initialized by `zero_boot_tables`
/// and advanced only by this allocator; a returned slot must not have
/// outstanding aliases.
/// Execute in the physical-address boot environment so `phys_addr_of!`
/// resolves this image's physical symbols.
#[unsafe(link_section = ".text.boot")]
unsafe fn alloc_l1_table() -> (*mut PageAligned<[u64; PT_ENTRIES]>, usize) {
    // SAFETY: early boot is the sole writer of the L1 table pool cursor.
    let idx = unsafe { NEXT_BOOT_L1_TABLE };
    assert!(idx < MAX_BOOT_L1_TABLES, "boot L1 table pool exhausted");
    // SAFETY: same single-writer early-boot invariant as above.
    unsafe {
        NEXT_BOOT_L1_TABLE += 1;
    }

    let pool = core::ptr::addr_of_mut!(BOOT_PT_L1_POOL).cast::<PageAligned<[u64; PT_ENTRIES]>>();
    // SAFETY: `idx < MAX_BOOT_L1_TABLES` was checked above, so this points to
    // an in-bounds table slot inside the static pool.
    let ptr = unsafe { pool.add(idx) };
    let pa = phys_addr_of!(BOOT_PT_L1_POOL) + idx * PAGE_SIZE_4K;
    // SAFETY: `ptr` names the newly allocated pool slot exclusively during
    // early boot, so zeroing it is valid.
    unsafe {
        *ptr = PageAligned::new([0; PT_ENTRIES]);
    }
    (ptr, pa)
}

/// # Safety
///
/// Call only during single-CPU boot, with exclusive access to the boot
/// tables and pool cursors, before hardware page walks use these tables.
/// `root_idx` must be below `PT_ENTRIES`; the root must have been zeroed,
/// and every populated root entry must name a table allocated from the L1
/// pool by this module. No reference may alias the returned table during
/// mutation.
/// Execute in the physical-address boot environment so `phys_addr_of!`
/// resolves this image's physical symbols.
#[unsafe(link_section = ".text.boot")]
unsafe fn root_l1_table(root_idx: usize) -> *mut PageAligned<[u64; PT_ENTRIES]> {
    // SAFETY: `root_idx` is derived from a Sv39 VPN index, so it is within the
    // root page-table bounds during boot mapping construction.
    if unsafe { BOOT_PT_L2[root_idx] } == 0 {
        // SAFETY: allocates a fresh L1 table from the boot-only pool.
        let (table, pa) = unsafe { alloc_l1_table() };
        // SAFETY: boot code is the sole mutator of the root table here.
        unsafe {
            BOOT_PT_L2[root_idx] = make_table_pte(pa);
        }
        table
    } else {
        // SAFETY: the root entry was previously initialized from a boot pool
        // table, so decoding its PTE yields the corresponding pool address.
        let pa = pte_paddr(unsafe { BOOT_PT_L2[root_idx] });
        let pool_base = phys_addr_of!(BOOT_PT_L1_POOL);
        let idx = (pa - pool_base) / PAGE_SIZE_4K;
        let pool =
            core::ptr::addr_of_mut!(BOOT_PT_L1_POOL).cast::<PageAligned<[u64; PT_ENTRIES]>>();
        // SAFETY: `idx` was recovered from a PTE that points into the static
        // boot L1 pool, so this recomputes that in-bounds pool slot.
        unsafe { pool.add(idx) }
    }
}

/// # Safety
///
/// Call only during single-CPU boot, with exclusive access to the boot
/// tables and pool cursors, before hardware page walks use these tables.
/// The pools must be initialized and all table links must point to their
/// corresponding pool slots. `va` and `pa` must be 2 MiB aligned; the
/// requested mapping and permissions must be valid for the boot memory
/// layout. The target slot must not contain a lower-level table.
/// Execute in the physical-address boot environment so `phys_addr_of!`
/// resolves this image's physical symbols.
#[expect(
    clippy::panic,
    reason = "CG-021-no-panic-on-reachable-failure: Boot callers map each VA window at one fixed \
              VA-to-PA offset (linear map at PAGE_OFFSET, kernel image at its image offset) and \
              the windows do not overlap, so a present entry for va already maps pa"
)]
#[unsafe(link_section = ".text.boot")]
unsafe fn map_2m_page(va: usize, pa: usize, flags: u64) {
    let root_idx = vpn2(va);
    let l1_idx = vpn1(va);
    // SAFETY: `root_idx` comes from the Sv39 VPN2 field of `va`, so it selects
    // the corresponding boot L1 table slot.
    let table = unsafe { root_l1_table(root_idx) };
    // SAFETY: `l1_idx` comes from the Sv39 VPN1 field of `va`, so indexing the
    // chosen L1 table at that slot is in bounds.
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

/// # Safety
///
/// Call only during single-CPU boot, with exclusive access to the boot
/// tables and pool cursors, before hardware page walks use these tables.
/// The pool-link invariants of `map_2m_page` must hold. `va` and `pa` must
/// have matching 2 MiB offsets, and rounding and address advancement must
/// not overflow. Mapping the entire rounded range with `flags` must be
/// valid for the boot layout.
/// Execute in the physical-address boot environment so `phys_addr_of!`
/// resolves this image's physical symbols.
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
        // SAFETY: the loop advances in 2 MiB steps across the caller-validated
        // boot mapping range and updates only boot page-table state.
        unsafe { map_2m_page(cur_va, cur_pa, flags) };
        cur_va += PAGE_SIZE_2M;
        cur_pa += PAGE_SIZE_2M;
    }
}

/// # Safety
///
/// Call only during single-CPU boot, with exclusive access to the boot
/// tables and pool cursors, before hardware page walks use these tables.
/// The tables must be initialized, and the configured boot-console address
/// must identify UART MMIO that may be mapped through its containing
/// 2 MiB blocks without conflicting with existing RAM mappings.
/// Execute in the physical-address boot environment so `phys_addr_of!`
/// resolves this image's physical symbols.
#[unsafe(link_section = ".text.boot")]
unsafe fn map_mmio_linear_ranges() {
    if BOOT_CONSOLE_TYPE == "mmio" && BOOT_CONSOLE_ADDR != 0 {
        // SAFETY: this installs the linear-map UART MMIO window into the
        // boot page tables before paging is enabled.
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

/// # Safety
///
/// Call only during single-CPU boot, with exclusive access to the boot
/// tables and pool cursors, before hardware page walks use these tables.
/// A nonzero `dtb_paddr` must point to a complete, readable and immutable
/// DTB in the physical boot mapping. Its memory regions must describe
/// actual RAM, and their linear addresses and 2 MiB rounding must not
/// overflow or conflict with the boot mappings.
/// Execute in the physical-address boot environment so `phys_addr_of!`
/// resolves this image's physical symbols.
#[unsafe(link_section = ".text.boot")]
unsafe fn map_boot_linear_ram_from_dtb(dtb_paddr: usize) {
    if dtb_paddr == 0 {
        return;
    }

    #[expect(
        clippy::panic,
        reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: the firmware DTB pointer does \
                  not reference a readable device tree, so boot RAM cannot be mapped; early \
                  page-table setup has no error return or recovery"
    )]
    // SAFETY: `dtb_paddr` is the boot firmware DTB pointer supplied to early
    // boot, and this helper only reads the fixed memory-region description from it.
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
        // SAFETY: each region comes from the validated DTB memory map and is
        // added only to the boot linear mapping.
        unsafe { map_range_2m(PAGE_OFFSET + start, start, region.size, PTE_RW) };
    }
}

/// # Safety
///
/// Call only during single-CPU boot, with exclusive access to the boot
/// tables and pool cursors, before hardware page walks use these tables.
/// The ordered physical bounds must describe the loaded kernel. The
/// identity, linear and `KIMAGE_VADDR` mappings must have matching 2 MiB
/// offsets and fit the address space after rounding, without conflicting
/// with existing mappings.
/// Execute in the physical-address boot environment so `phys_addr_of!`
/// resolves this image's physical symbols.
#[unsafe(link_section = ".text.boot")]
unsafe fn map_kernel_image(kernel_start_pa: usize, kernel_end_pa: usize) {
    let size = kernel_end_pa.saturating_sub(kernel_start_pa);
    // SAFETY: Single-core boot owns the tables; caller-supplied image bounds define this identity
    // mapping.
    unsafe { map_range_2m(kernel_start_pa, kernel_start_pa, size, PTE_RWX) };
    // SAFETY: The same image bounds and matching 2 MiB offsets define the dedicated kernel VA
    // mapping.
    unsafe { map_range_2m(KIMAGE_VADDR, kernel_start_pa, size, PTE_RWX) };
    // SAFETY: The caller guarantees the linear image range fits without overflow or mapping
    // conflicts.
    unsafe {
        map_range_2m(
            PAGE_OFFSET + kernel_start_pa,
            kernel_start_pa,
            size,
            PTE_RWX,
        )
    };
}

/// # Safety
///
/// Call only during single-CPU boot, with exclusive access to the boot
/// tables and pool cursors, before hardware page walks use these tables.
/// A nonzero `dtb_paddr` must identify the firmware DTB. Its linear address
/// and the rounded 2 MiB mapping range must not overflow or conflict with
/// existing boot mappings.
/// Execute in the physical-address boot environment so `phys_addr_of!`
/// resolves this image's physical symbols.
#[unsafe(link_section = ".text.boot")]
unsafe fn map_dtb_linear(dtb_paddr: usize) {
    if dtb_paddr == 0 {
        return;
    }
    // SAFETY: the boot DTB is immutable firmware data that must remain
    // reachable through the boot linear mapping.
    unsafe { map_range_2m(PAGE_OFFSET + dtb_paddr, dtb_paddr, PAGE_SIZE_2M, PTE_RW) };
}

#[unsafe(link_section = ".text.boot")]
/// # Safety
///
/// This function may only run during early single-CPU bring-up before paging
/// is enabled. The caller must ensure `dtb_paddr` is the boot firmware DTB for
/// the current machine image and that the boot page-table pools are not being
/// concurrently accessed.
pub(super) unsafe fn create_boot_page_tables(dtb_paddr: usize) {
    unsafe extern "C" {
        fn _start();
        fn _ekernel();
    }

    let kernel_start_pa = phys_addr_of!(_start);
    let kernel_end_pa = phys_addr_of!(_ekernel);

    // SAFETY: One-time single-CPU boot owns all inactive tables and pool cursors, with no
    // outstanding aliases.
    unsafe { zero_boot_tables() };
    // SAFETY: Fresh tables and linker-derived physical image bounds satisfy the image mapping
    // contract.
    unsafe { map_kernel_image(kernel_start_pa, kernel_end_pa) };
    // SAFETY: The firmware DTB remains readable and immutable; single-core boot owns the
    // initialized tables.
    unsafe { map_boot_linear_ram_from_dtb(dtb_paddr) };
    // SAFETY: The same firmware DTB must remain accessible through the linear map after paging
    // activation.
    unsafe { map_dtb_linear(dtb_paddr) };
    // SAFETY: The configured console identifies UART MMIO; the initialized boot tables remain
    // exclusively owned.
    unsafe { map_mmio_linear_ranges() };
}

#[unsafe(link_section = ".text.boot")]
/// # Safety
///
/// `BOOT_PT_L2` must already contain the fully initialized root page table for
/// the current boot image, and the caller must ensure switching to it keeps
/// the current execution path and required boot data mapped.
pub(super) unsafe fn init_mmu() {
    let root_pa = phys_addr_of!(BOOT_PT_L2);
    // SAFETY: `BOOT_PT_L2` was fully initialized by `create_boot_page_tables`,
    // and switching to it plus flushing the local TLB only affects the current hart.
    unsafe {
        karch::write_kernel_page_table(PhysAddr::from(root_pa).into());
        karch::flush_tlb(None);
    }
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Early boot page table setup and MMU initialisation for AArch64.
//!
//! All code in this module that runs before the MMU is enabled lives in
//! `.idmap.text` and uses only PC-relative addressing to obtain physical
//! addresses of data.
//!
//! Page table structure after [`create_boot_page_tables`]:
//!
//! ```text
//! TTBR0/TTBR1 → BOOT_PT_L0
//!   L0[0]   → BOOT_PT_L1        (early linear map for device/DTB access)
//!   L0[256] → BOOT_PT_L1_KIMAGE (kernel image area at KIMAGE_VADDR)
//!               └─ L1[0] → BOOT_PT_L2_KIMAGE (2 MiB blocks mapping PA(kernel) → KIMAGE_VADDR)
//! ```
//!
//! `KIMAGE_VADDR` is the fixed virtual address at which the kernel image is
//! linked.  The physical load address is detected at runtime via `adrp _start`.
//! This allows the kernel to be loaded at any physical address.

use aarch64_cpu::{asm::barrier, registers::*};
use kaddr_layout::KIMAGE_VADDR;
use kbuild_config::BOOT_DEVICE_MM;
use memaddr::PhysAddr;
use page_table::{
    PageTableEntry, PagingFlags,
    aarch64::{A64PageEntry, Arm64MemAttr},
};

/// Number of entries in each page table level (9-bit index for 4 KiB pages).
const PT_ENTRIES: usize = 512;

/// 1 GiB in bytes (L1 block granularity).
const GIB: usize = 0x4000_0000;

/// 2 MiB in bytes (L2 block granularity).
const MIB_2: usize = 0x20_0000;
/// Boot-time DTB RAM parser capacity.
///
/// Keep this in sync with platform-layer `MAX_RAM_REGIONS` values so boot-time
/// linear-map coverage and later `khal::mem::init()` region discovery don't
/// silently disagree about how many `/memory` ranges are supported.
const MAX_BOOT_RAM_REGIONS: usize = 16;

/// A page-aligned wrapper used to place page table arrays in the correct
/// linker section with the required 4 KiB alignment.
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

/// Level-0 boot page table (shared between TTBR0 and TTBR1).
#[unsafe(link_section = ".data.boot_page_table")]
static mut BOOT_PT_L0: PageAligned<[A64PageEntry; PT_ENTRIES]> =
    PageAligned::new([A64PageEntry::EMPTY; PT_ENTRIES]);

/// Level-1 page table for the early linear map.
///
/// L0[0] → this table; covers the first 512 GiB of physical address space.
#[unsafe(link_section = ".data.boot_page_table")]
static mut BOOT_PT_L1: PageAligned<[A64PageEntry; PT_ENTRIES]> =
    PageAligned::new([A64PageEntry::EMPTY; PT_ENTRIES]);

/// Level-1 page table for the KIMAGE virtual address region.
///
/// L0[256] → this table; covers 0xFFFF_8000_0000_0000 .. 0xFFFF_FFFF_FFFF_FFFF.
#[unsafe(link_section = ".data.boot_page_table")]
static mut BOOT_PT_L1_KIMAGE: PageAligned<[A64PageEntry; PT_ENTRIES]> =
    PageAligned::new([A64PageEntry::EMPTY; PT_ENTRIES]);

/// Level-2 page table for fine-grained (2 MiB) kernel image mapping.
///
/// L1_KIMAGE[0] → this table; maps up to 1 GiB at 2 MiB granularity.
#[unsafe(link_section = ".data.boot_page_table")]
static mut BOOT_PT_L2_KIMAGE: PageAligned<[A64PageEntry; PT_ENTRIES]> =
    PageAligned::new([A64PageEntry::EMPTY; PT_ENTRIES]);

/// Return the physical address of a static symbol via PC-relative `adrp`.
///
/// This macro must be used in `.idmap.text` code (before MMU is on) where all
/// symbol references must be physical addresses obtained via PC-relative
/// addressing.
macro_rules! phys_addr_of {
    ($sym:expr) => {{
        let pa: usize;
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

/// Build the boot page tables required to switch the MMU on.
///
/// Creates two sets of mappings:
///
/// 1. **Early linear map**:
///    - `BOOT_DEVICE_MM` → Device RW  (1 GiB blocks)
///    - DTB block → Normal RWX (1 GiB block if needed)
///      via `L0[0] → BOOT_PT_L1`
///
/// 2. **Kernel image map** (new):
///    - Physical kernel image (detected at runtime via `adrp`) →
///      `KIMAGE_VADDR` (2 MiB blocks)
///      via `L0[256] → BOOT_PT_L1_KIMAGE → BOOT_PT_L2_KIMAGE`
///
/// Both TTBR0 and TTBR1 point to the same L0 table.
///
/// # Safety
///
/// Must be called before the MMU is enabled.
#[unsafe(link_section = ".idmap.text")]
pub unsafe fn create_boot_page_tables() {
    // -----------------------------------------------------------------------
    // Get physical addresses of the page tables via PC-relative addressing.
    // -----------------------------------------------------------------------
    let l1_pa = phys_addr_of!(BOOT_PT_L1);
    let l1_kimage_pa = phys_addr_of!(BOOT_PT_L1_KIMAGE);
    let l2_kimage_pa = phys_addr_of!(BOOT_PT_L2_KIMAGE);

    // -----------------------------------------------------------------------
    // Get the actual physical load address of the kernel image at runtime.
    // Using adrp gives the physical address of _start since the bootloader
    // code runs in physical address space before the MMU is enabled.
    // -----------------------------------------------------------------------
    unsafe extern "C" {
        fn _start();
        fn _ekernel();
    }
    let kernel_start_pa = phys_addr_of!(_start);
    let kernel_end_pa = phys_addr_of!(_ekernel);

    // -----------------------------------------------------------------------
    // L0 setup
    // -----------------------------------------------------------------------
    unsafe {
        // L0[0] → L1 (linear map: first 512 GiB)
        BOOT_PT_L0[0] = A64PageEntry::new_table(PhysAddr::from(l1_pa));

        // L0[256] → L1_KIMAGE (KIMAGE region: 0xFFFF_8000_0000_0000 +)
        // KIMAGE_VADDR = 0xFFFF_8000_0000_0000; L0 index (via TTBR1) = bit[47:39]
        // of (KIMAGE_VADDR & 0x0000_FFFF_FFFF_FFFF) = 0x0000_8000_0000_0000 >> 39 = 256.
        let kimage_l0_idx = (KIMAGE_VADDR & !0xFFFF_0000_0000_0000) >> 39;
        BOOT_PT_L0[kimage_l0_idx] = A64PageEntry::new_table(PhysAddr::from(l1_kimage_pa));
    }

    // -----------------------------------------------------------------------
    // Linear map: device memory (1 GiB blocks) via L1
    // -----------------------------------------------------------------------
    for &(start, end) in BOOT_DEVICE_MM {
        let mut addr = start;
        while addr < end {
            unsafe {
                BOOT_PT_L1[addr / GIB] = A64PageEntry::new_page(
                    PhysAddr::from(addr),
                    PagingFlags::READ | PagingFlags::WRITE | PagingFlags::DEVICE,
                    true, // 1 GiB block
                );
            }
            addr += GIB;
        }
    }

    // -----------------------------------------------------------------------
    // Early normal-memory fallbacks:
    //
    // 1. Map the current kernel physical block so execution can continue at the
    //    current low physical PC immediately after the MMU is enabled, before
    //    we branch to the high KIMAGE virtual address.
    // 2. Map the firmware-provided DTB block so early_init() can consume the
    //    device tree before the runtime linear map is rebuilt.
    //
    // Without the explicit kernel-block fallback, platforms whose firmware
    // places the kernel outside BOOT_DEVICE_MM (for example crosvm) will fault
    // as soon as SCTLR_EL1.M is set because the current low PC stops being
    // translated.
    // -----------------------------------------------------------------------
    let kernel_l1_idx = kernel_start_pa / GIB;
    assert!(
        kernel_l1_idx < PT_ENTRIES,
        "kernel outside early linear map window"
    );
    if !unsafe { BOOT_PT_L1[kernel_l1_idx].is_present() } {
        let block_base = kernel_start_pa & !(GIB - 1);
        unsafe {
            BOOT_PT_L1[kernel_l1_idx] = A64PageEntry::new_page(
                PhysAddr::from(block_base),
                PagingFlags::READ | PagingFlags::WRITE | PagingFlags::EXECUTE,
                true,
            );
        }
    }

    let dtb_paddr = unsafe { super::entry::SAVED_BOOT_ARGS[0] as usize };
    if dtb_paddr != 0 {
        let dtb_l1_idx = dtb_paddr / GIB;
        assert!(
            dtb_l1_idx < PT_ENTRIES,
            "DTB outside early linear map window"
        );
        if !unsafe { BOOT_PT_L1[dtb_l1_idx].is_present() } {
            let block_base = dtb_paddr & !(GIB - 1);
            unsafe {
                BOOT_PT_L1[dtb_l1_idx] = A64PageEntry::new_page(
                    PhysAddr::from(block_base),
                    PagingFlags::READ | PagingFlags::WRITE | PagingFlags::EXECUTE,
                    true,
                );
            }
        }
    }

    // -----------------------------------------------------------------------
    // KIMAGE map: L1_KIMAGE[0] → L2_KIMAGE
    // L1 index for KIMAGE_VADDR: bit[38:30] of (KIMAGE_VADDR & 0x0000_FFFF_FFFF_FFFF)
    // = 0x0000_8000_0000_0000 bits[38:30] = 0, so L1_KIMAGE[0].
    // -----------------------------------------------------------------------
    unsafe {
        let kimage_l1_idx = (KIMAGE_VADDR & !0xFFFF_0000_0000_0000) >> 30 & (PT_ENTRIES - 1);
        BOOT_PT_L1_KIMAGE[kimage_l1_idx] = A64PageEntry::new_table(PhysAddr::from(l2_kimage_pa));
    }

    // -----------------------------------------------------------------------
    // KIMAGE map: 2 MiB blocks in L2 mapping physical kernel to KIMAGE_VADDR
    // -----------------------------------------------------------------------
    // Round kernel_start_pa down to 2 MiB alignment.
    let pa_base = kernel_start_pa & !(MIB_2 - 1);
    // Round kernel_end_pa up to 2 MiB alignment.
    let pa_end = (kernel_end_pa + MIB_2 - 1) & !(MIB_2 - 1);

    let mut pa = pa_base;
    let mut l2_idx = 0usize;
    while pa < pa_end {
        unsafe {
            BOOT_PT_L2_KIMAGE[l2_idx] = A64PageEntry::new_page(
                PhysAddr::from(pa),
                PagingFlags::READ | PagingFlags::WRITE | PagingFlags::EXECUTE,
                true, // 2 MiB block
            );
        }
        pa += MIB_2;
        l2_idx += 1;
    }

    // Ensure all page table writes complete before enabling the MMU.
    barrier::dsb(barrier::SY);
}

fn map_linear_block(block_base: usize, flags: PagingFlags) {
    let l1_idx = block_base / GIB;
    assert!(
        l1_idx < PT_ENTRIES,
        "physical address outside early linear map window"
    );
    unsafe {
        // Preserve any boot-time mapping that is already present. In
        // particular, the block containing the kernel image / secondary
        // trampoline must remain executable until all CPUs have completed the
        // early boot path.
        if BOOT_PT_L1[l1_idx].is_present() {
            return;
        }
        BOOT_PT_L1[l1_idx] = A64PageEntry::new_page(PhysAddr::from(block_base), flags, true);
    }
}

pub fn map_boot_linear_ram_from_dtb(dtb_paddr: usize) {
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
        let start = region.starting_address as usize & !(GIB - 1);
        let end = ((region.starting_address as usize) + region.size + GIB - 1) & !(GIB - 1);
        let mut addr = start;
        while addr < end {
            map_linear_block(addr, PagingFlags::READ | PagingFlags::WRITE);
            addr += GIB;
        }
    }

    barrier::dsb(barrier::SY);
    karch::flush_tlb(None);
    barrier::isb(barrier::SY);
}

/// Configure MMU registers and enable the MMU.
///
/// Sets `MAIR_EL1`, `TCR_EL1`, `TTBR0_EL1`, `TTBR1_EL1` and then turns
/// the MMU on via `SCTLR_EL1`.
///
/// # Safety
///
/// Must be called after [`create_boot_page_tables`] and before any code
/// that relies on virtual addresses.
#[unsafe(link_section = ".idmap.text")]
pub unsafe fn init_mmu() {
    // Obtain physical address of L0 page table via PC-relative addressing.
    let root_pa = phys_addr_of!(BOOT_PT_L0);

    // Program memory attributes.
    MAIR_EL1.set(Arm64MemAttr::MAIR_VALUE);

    // Configure TCR_EL1: 4 KiB granule, 48-bit VA, 48-bit PA, inner-shareable
    // write-back cacheable walks for both TTBR0 (T0SZ=16) and TTBR1 (T1SZ=16).
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
    TCR_EL1.write(TCR_EL1::IPS::Bits_48 + tcr_flags0 + tcr_flags1);
    barrier::isb(barrier::SY);

    // Point both TTBR0 and TTBR1 at the same L0 table so that low (identity)
    // and high (kernel) virtual addresses are both accessible right after the
    // MMU is enabled.
    let root_pa_u64 = root_pa as u64;
    TTBR0_EL1.set(root_pa_u64);
    TTBR1_EL1.set(root_pa_u64);

    // Flush the entire TLB before enabling the MMU.
    karch::flush_tlb(None);

    // Enable the MMU and turn on I-cache and D-cache.
    SCTLR_EL1.modify(SCTLR_EL1::M::Enable + SCTLR_EL1::C::Cacheable + SCTLR_EL1::I::Cacheable);
    // Disable SPAN
    SCTLR_EL1.set(SCTLR_EL1.get() | (1 << 23));
    barrier::isb(barrier::SY);

    unsafe extern "C" {
        fn _start();
    }
    let start_pa: usize = phys_addr_of!(_start);
    super::serial::boot_print_str("[boot] kernel start PA: ");
    super::serial::boot_print_usize(start_pa);
    super::serial::boot_print_str(", KIMAGE_VADDR: ");
    super::serial::boot_print_usize(KIMAGE_VADDR);
    super::serial::boot_print_str("\r\n");
}

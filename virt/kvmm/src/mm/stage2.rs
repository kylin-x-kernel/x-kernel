// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! AArch64 Stage-2 page table (IPA → PA) for guest physical memory.
//!
//! Uses VTCR_EL2 with T0SZ=32 (32-bit IPA space = 4 GiB) and SL0=1
//! (start at Level 1). The page table is an identity map (IPA = PA)
//! with 2 MiB block entries at Level 2.
//!
//! Layout:
//! ```text
//! L1: 4 entries × 1 GiB   → covers [0, 4 GiB)
//! L2: 4 × 512 entries × 2 MiB blocks
//! ```

use super::{GuestMem, GuestPerm, free_pt_page};

const L1_ENTRIES: usize = 4;
const L2_ENTRIES: usize = 512;
const L3_ENTRIES: usize = 512;

// LPAE descriptor bits (Stage-2 format)
const LPAE_VALID: u64 = 1 << 0;
const LPAE_TABLE: u64 = 1 << 1;
const LPAE_AF: u64 = 1 << 10;
const LPAE_SH_IS: u64 = 3 << 8;
const LPAE_MATTR_NORM: u64 = 0xF << 2; // Normal WB cacheable
const LPAE_MATTR_DEV: u64 = 0x1 << 2; // Device-nGnRE
const LPAE_XN: u64 = 1 << 54;
const LPAE_S2AP_RW: u64 = 3 << 6;

const LPAE_ADDR_MASK: u64 = 0x0000_FFFF_FFFF_F000;

// VTCR_EL2 field constructors
const fn vtcr_t0sz(n: u64) -> u64 {
    n & 0x3f
}
const fn vtcr_sl0(n: u64) -> u64 {
    (n & 0x3) << 6
}
const VTCR_IRGN0_WBWA: u64 = 1 << 8;
const VTCR_ORGN0_WBWA: u64 = 1 << 10;
const VTCR_SH0_IS: u64 = 3 << 12;
const VTCR_TG0_4K: u64 = 0 << 14;
const VTCR_PS_36BITS: u64 = 1 << 16;

const VTTBR_VMID_SHIFT: u64 = 48;

/// AArch64 Stage-2 page table.
///
/// Owns the root L1 page. On drop, walks L1 entries to free L2 pages,
/// then the L1 page itself is freed by `GlobalPage::drop`.
pub struct Stage2 {
    l1_page: kalloc::GlobalPage,
    vmid: u32,
}

impl Stage2 {
    fn root_pa(&self) -> u64 {
        kaddr_layout::v2p(self.l1_page.as_ptr() as usize) as u64
    }

    fn destroy(&mut self) {
        let l1_va = self.l1_page.as_ptr() as *const u64;
        for i in 0..L1_ENTRIES {
            // SAFETY: l1_va is valid; i < L1_ENTRIES.
            let entry = unsafe { l1_va.add(i).read() };
            if entry & LPAE_VALID != 0 && entry & LPAE_TABLE != 0 {
                let l2_pa = entry & LPAE_ADDR_MASK;
                let l2_va = kaddr_layout::p2v(l2_pa as usize) as *const u64;

                // Free any L3 tables split out of this L2 (e.g. GICC→GICV).
                for j in 0..L2_ENTRIES {
                    // SAFETY: l2_va is valid; j < L2_ENTRIES.
                    let l2_entry = unsafe { l2_va.add(j).read() };
                    if l2_entry & LPAE_VALID != 0 && l2_entry & LPAE_TABLE != 0 {
                        let l3_pa = l2_entry & LPAE_ADDR_MASK;
                        // SAFETY: l3_pa was allocated by ensure_l3_table.
                        unsafe { free_pt_page(l3_pa) };
                    }
                }

                // SAFETY: l2_pa was allocated by us in new().
                unsafe { free_pt_page(l2_pa) };
            }
        }
    }

    /// Ensure the L2 slot `(l1_idx, l2_idx)` is an L3 table, returning its VA.
    ///
    /// If the slot is already a table, reuses it. If it is a 2 MiB block,
    /// splits it into 512 identity 4 KiB pages. If it is invalid, allocates a
    /// zeroed (all-invalid) L3 table. Returns null on allocation failure.
    fn ensure_l3_table(&mut self, l1_idx: usize, l2_idx: usize) -> *mut u64 {
        let l1_va = self.l1_page.as_ptr() as *const u64;
        // SAFETY: l1_va valid, l1_idx < L1_ENTRIES (checked by caller).
        let l1_entry = unsafe { l1_va.add(l1_idx).read() };
        if l1_entry & LPAE_VALID == 0 {
            return core::ptr::null_mut();
        }

        let l2_pa = l1_entry & LPAE_ADDR_MASK;
        let l2_va = kaddr_layout::p2v(l2_pa as usize) as *mut u64;
        // SAFETY: l2_va valid, l2_idx < L2_ENTRIES.
        let l2_entry = unsafe { l2_va.add(l2_idx).read() };

        if l2_entry & LPAE_VALID != 0 && l2_entry & LPAE_TABLE != 0 {
            let l3_pa = l2_entry & LPAE_ADDR_MASK;
            return kaddr_layout::p2v(l3_pa as usize) as *mut u64;
        }

        let mut l3_page = match kalloc::GlobalPage::alloc_zero() {
            Ok(p) => p,
            Err(_) => return core::ptr::null_mut(),
        };
        let l3_va = l3_page.as_mut_ptr() as *mut u64;
        let l3_pa = kaddr_layout::v2p(l3_va as usize) as u64;
        core::mem::forget(l3_page);

        if l2_entry & LPAE_VALID != 0 {
            // 2 MiB block → populate the L3 with 512 identity 4 KiB pages so
            // the existing mapping is preserved before we overwrite one page.
            let block_pa = l2_entry & LPAE_ADDR_MASK;
            let block_attr = l2_entry & !LPAE_ADDR_MASK & !0x3u64;
            for i in 0..L3_ENTRIES {
                let page_pa = block_pa + (i as u64) * 4096;
                // SAFETY: l3_va is a freshly-allocated zeroed page.
                unsafe {
                    l3_va
                        .add(i)
                        .write(page_pa | block_attr | LPAE_VALID | LPAE_TABLE);
                }
            }
        }
        // else: invalid L2 → leave the L3 all-invalid (region keeps trapping).

        flush_pt_page(l3_va as *const u8);

        // SAFETY: install the L3 table into the L2 slot after it is flushed.
        unsafe {
            l2_va.add(l2_idx).write(l3_pa | LPAE_VALID | LPAE_TABLE);
            core::arch::asm!("dc civac, {}", in(reg) l2_va.add(l2_idx));
            core::arch::asm!("dsb ish");
        }
        flush_stage2_tlb();
        l3_va
    }
}

impl Drop for Stage2 {
    fn drop(&mut self) {
        self.destroy();
    }
}

impl GuestMem for Stage2 {
    fn new(mem_base: u64, mem_size: u64, vmid: u32) -> Option<Self> {
        let mem_end = mem_base.checked_add(mem_size)?;

        // Allocate L1 page (4 entries, but we need a full 4KB page)
        let mut l1_page = kalloc::GlobalPage::alloc_zero().ok()?;
        let l1_va = l1_page.as_mut_ptr() as *mut u64;
        let l1_pa = kaddr_layout::v2p(l1_va as usize) as u64;

        // Allocate 4 L2 pages (one per L1 entry)
        let mut l2_pas = [0u64; L1_ENTRIES];
        for l2_pa in &mut l2_pas {
            let mut page = kalloc::GlobalPage::alloc_zero().ok()?;
            let va = page.as_mut_ptr() as usize;
            *l2_pa = kaddr_layout::v2p(va) as u64;
            core::mem::forget(page);
        }

        // Fill L1 → L2 table entries
        for (i, &l2_pa) in l2_pas.iter().enumerate() {
            // SAFETY: l1_va is a valid 4KB-aligned page we own.
            unsafe {
                l1_va.add(i).write(l2_pa | LPAE_VALID | LPAE_TABLE);
            }
        }

        // Fill L2 block entries. Only guest RAM is mapped (Normal, RW).
        // Everything else — device MMIO (GIC, UART, ...) and unbacked IPA — is
        // left INVALID so the guest faults to EL2 and the VMM emulates it. The
        // guest must never reach real host devices; the only exception is added
        // later via `map_region` (GICC → hardware GICV).
        for (i1, &l2_pa) in l2_pas.iter().enumerate() {
            let l2_va = kaddr_layout::p2v(l2_pa as usize) as *mut u64;
            for i2 in 0..L2_ENTRIES {
                let ipa = ((i1 as u64) << 30) | ((i2 as u64) << 21);
                let is_ram = ipa >= mem_base && ipa < mem_end;
                let entry = if is_ram {
                    ipa | LPAE_AF | LPAE_SH_IS | LPAE_MATTR_NORM | LPAE_S2AP_RW | LPAE_VALID
                } else {
                    0 // invalid → Stage-2 fault → trap to VMM MMIO emulation
                };
                // SAFETY: l2_va is a valid 4KB-aligned page; i2 < 512.
                unsafe {
                    l2_va.add(i2).write(entry);
                }
            }
        }

        // Flush page table pages from data cache
        flush_pt_page(l1_va as *const u8);
        for pa in &l2_pas {
            flush_pt_page(kaddr_layout::p2v(*pa as usize) as *const u8);
        }

        let vtcr = vtcr_t0sz(32)
            | vtcr_sl0(1)
            | VTCR_TG0_4K
            | VTCR_SH0_IS
            | VTCR_IRGN0_WBWA
            | VTCR_ORGN0_WBWA
            | VTCR_PS_36BITS;

        log::info!(
            "[stage2] init mem={:#x}+{:#x} l1_pa={:#x} vtcr={:#x}",
            mem_base,
            mem_size,
            l1_pa,
            vtcr,
        );

        Some(Self { l1_page, vmid })
    }

    fn map_region(&mut self, gpa: u64, hpa: u64, size: u64, perm: GuestPerm) -> bool {
        let start = gpa & !0xFFF;
        let hpa_base = hpa & !0xFFF;
        let end = (gpa + size + 0xFFF) & !0xFFF;

        let attr = match perm {
            GuestPerm::RamRW => LPAE_AF | LPAE_SH_IS | LPAE_MATTR_NORM | LPAE_S2AP_RW,
            GuestPerm::DeviceRW => LPAE_AF | LPAE_MATTR_DEV | LPAE_S2AP_RW | LPAE_XN,
        };

        let mut offset: u64 = 0;
        while start + offset < end {
            let cur_gpa = start + offset;
            let cur_hpa = hpa_base + offset;

            let l1_idx = (cur_gpa >> 30) as usize;
            let l2_idx = ((cur_gpa >> 21) & 0x1FF) as usize;
            let l3_idx = ((cur_gpa >> 12) & 0x1FF) as usize;

            if l1_idx >= L1_ENTRIES {
                return false;
            }

            let l3_va = self.ensure_l3_table(l1_idx, l2_idx);
            if l3_va.is_null() {
                return false;
            }

            // SAFETY: l3_va points to a valid L3 page; l3_idx < 512.
            unsafe {
                let entry_ptr = l3_va.add(l3_idx);
                entry_ptr.write(cur_hpa | attr | LPAE_VALID | LPAE_TABLE);
                core::arch::asm!("dc civac, {}", in(reg) entry_ptr);
            }

            offset += 4096;
        }

        // SAFETY: barrier + TLB invalidation.
        unsafe { core::arch::asm!("dsb ish") };
        flush_stage2_tlb();

        log::info!(
            "[stage2] map_region gpa={:#x} → hpa={:#x} size={:#x}",
            gpa,
            hpa,
            size,
        );
        true
    }

    fn gpa_to_hpa(&self, gpa: u64) -> Option<u64> {
        // Identity map: GPA = HPA within the 4 GiB window
        if gpa < (L1_ENTRIES as u64) << 30 {
            Some(gpa)
        } else {
            None
        }
    }

    fn activate(&self) {
        let vttbr = self.root_pa() | ((self.vmid as u64) << VTTBR_VMID_SHIFT);
        let vtcr = vtcr_t0sz(32)
            | vtcr_sl0(1)
            | VTCR_TG0_4K
            | VTCR_SH0_IS
            | VTCR_IRGN0_WBWA
            | VTCR_ORGN0_WBWA
            | VTCR_PS_36BITS;
        // SAFETY: writing VTCR/VTTBR/HCR from EL2 is safe; root page is valid.
        unsafe {
            core::arch::asm!(
                "msr vtcr_el2, {vtcr}",
                "msr vttbr_el2, {vttbr}",
                "mrs {tmp}, hcr_el2",
                "orr {tmp}, {tmp}, #1",
                "msr hcr_el2, {tmp}",
                "isb",
                vtcr = in(reg) vtcr,
                vttbr = in(reg) vttbr,
                tmp = out(reg) _,
            );
        }
    }

    fn unmap_range(&mut self, gpa: u64, size: u64) -> bool {
        let block_size: u64 = 1 << 21; // 2 MiB
        let start = gpa & !(block_size - 1);
        let end = (gpa + size + block_size - 1) & !(block_size - 1);

        let l1_va = self.l1_page.as_ptr() as *const u64;

        let mut addr = start;
        while addr < end {
            let l1_idx = (addr >> 30) as usize;
            let l2_idx = ((addr >> 21) & 0x1FF) as usize;

            if l1_idx >= L1_ENTRIES {
                return false;
            }

            // SAFETY: l1_va is valid; l1_idx < L1_ENTRIES.
            let l1_entry = unsafe { l1_va.add(l1_idx).read() };
            if l1_entry & LPAE_VALID == 0 {
                addr += block_size;
                continue;
            }

            let l2_pa = l1_entry & LPAE_ADDR_MASK;
            let l2_va = kaddr_layout::p2v(l2_pa as usize) as *mut u64;
            // SAFETY: l2_va points to a valid L2 page; l2_idx < 512.
            unsafe {
                let entry_ptr = l2_va.add(l2_idx);
                entry_ptr.write(0);
                // Flush the cache line so the table walker sees the invalidation.
                core::arch::asm!("dc civac, {}", in(reg) entry_ptr);
                core::arch::asm!("dsb ish");
            }

            addr += block_size;
        }

        flush_stage2_tlb();
        true
    }
}

fn flush_pt_page(va: *const u8) {
    let start = va as u64 & !63;
    let end = start + 4096;
    let mut p = start;
    while p < end {
        // SAFETY: flushing cache lines within a known-valid 4KB page.
        unsafe {
            core::arch::asm!("dc civac, {}", in(reg) p);
        }
        p += 64;
    }
    // SAFETY: barrier instructions.
    unsafe {
        core::arch::asm!("dsb sy");
    }
    flush_stage2_tlb();
}

fn flush_stage2_tlb() {
    // SAFETY: TLB invalidation is safe from EL2.
    unsafe {
        core::arch::asm!("tlbi vmalls12e1is", "dsb ish", "isb",);
    }
}

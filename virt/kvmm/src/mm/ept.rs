// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! x86_64 Extended Page Table (EPT) for guest physical memory.
//!
//! EPT uses a 4-level page table (PML4 → PDPT → PD → PT) with a
//! different PTE format from the regular x86_64 paging:
//!
//! | Bits  | Meaning                            |
//! |-------|------------------------------------|
//! | 0     | Read access                        |
//! | 1     | Write access                       |
//! | 2     | Execute access                     |
//! | 3-5   | Memory type (0=UC, 6=WB)           |
//! | 6     | Ignore PAT                         |
//! | 7     | Large page (at PD level = 2 MiB)   |
//! | 12-51 | Physical address of next level/page|
//!
//! We build an identity map using 2 MiB large pages at PD level
//! to cover `[0, 4 GiB)`.

use super::{GuestMem, GuestPerm, free_pt_page};

// EPT PTE permission bits
const EPT_READ: u64 = 1 << 0;
const EPT_WRITE: u64 = 1 << 1;
const EPT_EXEC: u64 = 1 << 2;
const EPT_RWX: u64 = EPT_READ | EPT_WRITE | EPT_EXEC;

// EPT memory type (bits 5:3)
const EPT_MT_UC: u64 = 0 << 3; // Uncacheable
const EPT_MT_WB: u64 = 6 << 3; // Write-Back

// EPT large page bit (PD-level 2 MiB entry)
const EPT_LARGE: u64 = 1 << 7;

// Mask to extract physical address from an EPT entry (bits 12-51)
const EPT_ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

const L1_ENTRIES: usize = 4; // PDPT entries covering 4 GiB
const L2_ENTRIES: usize = 512; // PD entries per PDPT entry (2 MiB each)

/// x86_64 Extended Page Table.
///
/// Owns the root PML4 page. On drop, walks PML4 → PDPT → PD and frees
/// all sub-pages, then the PML4 page itself is freed by `GlobalPage::drop`.
pub struct Ept {
    pml4_page: kalloc::GlobalPage,
}

impl Ept {
    /// Construct the EPTP value for VMCS programming.
    ///
    /// Format: `root_pa | (page_walk_length - 1) << 3 | memory_type`
    /// - page walk length = 4 (4-level), so field = 3
    /// - memory type = 6 (WB) for the EPT paging structures themselves
    pub fn eptp(&self) -> u64 {
        let pml4_pa = kaddr_layout::v2p(self.pml4_page.as_ptr() as usize) as u64;
        pml4_pa | (3 << 3) | 6
    }

    fn destroy(&mut self) {
        let pml4_va = self.pml4_page.as_ptr() as *const u64;
        // SAFETY: pml4_va points to a valid 4KB page.
        let pdpt_entry = unsafe { pml4_va.read() };
        if pdpt_entry & EPT_READ == 0 {
            return;
        }

        let pdpt_pa = pdpt_entry & EPT_ADDR_MASK;
        let pdpt_ptr = kaddr_layout::p2v(pdpt_pa as usize) as *const u64;

        for i in 0..L1_ENTRIES {
            // SAFETY: pdpt_ptr points to a valid 4KB page; i < 512.
            let pd_entry = unsafe { pdpt_ptr.add(i).read() };
            if pd_entry & EPT_READ != 0 {
                let pd_pa = pd_entry & EPT_ADDR_MASK;
                // SAFETY: pd_pa was allocated by us in new().
                unsafe { free_pt_page(pd_pa) };
            }
        }
        // SAFETY: pdpt_pa was allocated by us in new().
        unsafe { free_pt_page(pdpt_pa) };
    }
}

impl Drop for Ept {
    fn drop(&mut self) {
        self.destroy();
    }
}

impl GuestMem for Ept {
    fn new(mem_base: u64, mem_size: u64, vmid: u32) -> Option<Self> {
        let _ = vmid;
        let mem_end = mem_base.checked_add(mem_size)?;

        // Allocate PML4 page
        let mut pml4_page = kalloc::GlobalPage::alloc_zero().ok()?;
        let pml4_va = pml4_page.as_mut_ptr() as *mut u64;
        let pml4_pa = kaddr_layout::v2p(pml4_va as usize) as u64;

        // Allocate PDPT page
        let mut pdpt_page = kalloc::GlobalPage::alloc_zero().ok()?;
        let pdpt_va = pdpt_page.as_mut_ptr() as *mut u64;
        let pdpt_pa = kaddr_layout::v2p(pdpt_va as usize) as u64;
        core::mem::forget(pdpt_page);

        // PML4[0] → PDPT
        // SAFETY: pml4_va is a valid zeroed 4KB page.
        unsafe {
            pml4_va.write(pdpt_pa | EPT_RWX);
        }

        // Allocate 4 PD pages (one per PDPT entry, covering 1 GiB each)
        for i in 0..L1_ENTRIES {
            let mut pd_page = kalloc::GlobalPage::alloc_zero().ok()?;
            let pd_va = pd_page.as_mut_ptr() as *mut u64;
            let pd_pa = kaddr_layout::v2p(pd_va as usize) as u64;
            core::mem::forget(pd_page);

            // PDPT[i] → PD[i]
            // SAFETY: pdpt_va is valid; i < 512.
            unsafe {
                pdpt_va.add(i).write(pd_pa | EPT_RWX);
            }

            // Fill PD with 2 MiB large page entries
            for j in 0..L2_ENTRIES {
                let gpa = ((i as u64) << 30) | ((j as u64) << 21);
                let is_ram = gpa >= mem_base && gpa < mem_end;
                let mt = if is_ram { EPT_MT_WB } else { EPT_MT_UC };
                let entry = gpa | EPT_RWX | mt | EPT_LARGE;
                // SAFETY: pd_va is valid; j < 512.
                unsafe {
                    pd_va.add(j).write(entry);
                }
            }
        }

        log::info!(
            "[ept] init mem={:#x}+{:#x} pml4_pa={:#x}",
            mem_base,
            mem_size,
            pml4_pa,
        );

        Some(Self { pml4_page })
    }

    fn map_region(&mut self, _gpa: u64, _hpa: u64, _size: u64, _perm: GuestPerm) -> bool {
        false
    }

    fn gpa_to_hpa(&self, gpa: u64) -> Option<u64> {
        if gpa < (L1_ENTRIES as u64) << 30 {
            Some(gpa)
        } else {
            None
        }
    }

    fn activate(&self) {
        // EPT activation is done via VMCS fields, not a direct register write.
        // The EPTP is written to the VMCS during vmcs_init_vcpu.
        // This method is intentionally a no-op; the caller uses eptp() to
        // obtain the value and writes it to the VMCS.
    }

    fn unmap_range(&mut self, gpa: u64, size: u64) -> bool {
        let block_size: u64 = 1 << 21; // 2 MiB
        let start = gpa & !(block_size - 1);
        let end = (gpa + size + block_size - 1) & !(block_size - 1);

        let pml4_va = self.pml4_page.as_ptr() as *const u64;
        // SAFETY: pml4_va is valid; entry 0 holds the PDPT pointer.
        let pdpt_entry = unsafe { pml4_va.read() };
        if pdpt_entry & EPT_READ == 0 {
            return false;
        }
        let pdpt_pa = pdpt_entry & EPT_ADDR_MASK;
        let pdpt_ptr = kaddr_layout::p2v(pdpt_pa as usize) as *const u64;

        let mut addr = start;
        while addr < end {
            let pdpt_idx = (addr >> 30) as usize;
            let pd_idx = ((addr >> 21) & 0x1FF) as usize;

            if pdpt_idx >= L1_ENTRIES {
                return false;
            }

            // SAFETY: pdpt_ptr is valid; pdpt_idx < L1_ENTRIES.
            let pd_entry = unsafe { pdpt_ptr.add(pdpt_idx).read() };
            if pd_entry & EPT_READ == 0 {
                addr += block_size;
                continue;
            }

            let pd_pa = pd_entry & EPT_ADDR_MASK;
            let pd_va = kaddr_layout::p2v(pd_pa as usize) as *mut u64;
            // SAFETY: pd_va is valid; pd_idx < 512.
            unsafe {
                pd_va.add(pd_idx).write(0);
            }

            addr += block_size;
        }
        true
    }
}

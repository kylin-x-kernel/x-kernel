// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! RISC-V G-stage page table (GPA → HPA) for guest physical memory.
//!
//! Uses the H-extension `hgatp` CSR to enable second-stage address
//! translation. The PTE format is identical to Sv39 (V/R/W/X/U/G/A/D),
//! only the address space meaning changes from VA→PA to GPA→HPA.
//!
//! We use Sv39x4 mode: the root page table is 16 KiB (4 contiguous
//! 4 KiB pages = 2048 entries), each entry covers 1 GiB, giving a
//! 2 TiB guest physical address range. For the 4 GiB identity map
//! we only need the first 4 entries, each pointing to a L1 table
//! that uses 2 MiB superpage entries.

use super::{GuestMem, GuestPerm, free_pt_page};

// RISC-V PTE bits (same for S-stage and G-stage)
const PTE_V: u64 = 1 << 0;
const PTE_R: u64 = 1 << 1;
const PTE_W: u64 = 1 << 2;
const PTE_X: u64 = 1 << 3;
const PTE_U: u64 = 1 << 4;
const PTE_A: u64 = 1 << 6;
const PTE_D: u64 = 1 << 7;

const L2_ENTRIES: usize = 4; // Root entries covering 4 GiB
const L1_ENTRIES: usize = 512; // Entries per L1 table (2 MiB each)

// hgatp mode field
const HGATP_MODE_SV39X4: u64 = 8 << 60;
const HGATP_VMID_SHIFT: u64 = 44;

/// RISC-V G-stage page table.
///
/// Owns the root contiguous page (4 × 4 KiB = 16 KiB). On drop,
/// walks root entries to free L1 pages, then the root page itself
/// is freed by `GlobalPage::drop`.
pub struct GStage {
    root_page: kalloc::GlobalPage,
    vmid: u32,
}

impl GStage {
    fn root_pa(&self) -> u64 {
        kaddr_layout::v2p(self.root_page.as_ptr() as usize) as u64
    }

    fn destroy(&mut self) {
        let root_va = self.root_page.as_ptr() as *const u64;
        // Only first L2_ENTRIES (4) are populated, but scan all valid entries.
        for i in 0..L2_ENTRIES {
            // SAFETY: root_va points to 16 KiB; i < 2048.
            let entry = unsafe { root_va.add(i).read() };
            // Non-leaf table entry: V=1, R=W=X=0
            if entry & PTE_V != 0 && (entry & (PTE_R | PTE_W | PTE_X)) == 0 {
                let l1_pa = (entry >> 10) << 12;
                // SAFETY: l1_pa was allocated by us in new().
                unsafe { free_pt_page(l1_pa) };
            }
        }
    }
}

impl Drop for GStage {
    fn drop(&mut self) {
        self.destroy();
    }
}

impl GuestMem for GStage {
    fn new(mem_base: u64, mem_size: u64, vmid: u32) -> Option<Self> {
        let mem_end = mem_base.checked_add(mem_size)?;

        // Sv39x4 root: 4 contiguous 4KB pages (16KB, 16KB-aligned)
        let mut root_page = kalloc::GlobalPage::alloc_contiguous(4, 4 * 4096).ok()?;
        let root_va = root_page.as_mut_ptr() as *mut u64;
        let root_pa = kaddr_layout::v2p(root_va as usize) as u64;

        // Zero the full 16KB root
        // SAFETY: root_va points to 16KB of owned memory.
        unsafe {
            core::ptr::write_bytes(root_va, 0, 2048);
        }

        // Allocate 4 L1 tables and fill root entries
        for i in 0..L2_ENTRIES {
            let mut l1_page = kalloc::GlobalPage::alloc_zero().ok()?;
            let l1_va = l1_page.as_mut_ptr() as *mut u64;
            let l1_pa = kaddr_layout::v2p(l1_va as usize) as u64;
            core::mem::forget(l1_page);

            // Root entry: non-leaf (V=1, R=W=X=0) pointing to L1
            let root_pte = ((l1_pa >> 12) << 10) | PTE_V;
            // SAFETY: root_va is valid; i < 2048.
            unsafe {
                root_va.add(i).write(root_pte);
            }

            // Fill L1 with 2MB superpage entries (V+R+W+X+A+D = leaf)
            for j in 0..L1_ENTRIES {
                let gpa = ((i as u64) << 30) | ((j as u64) << 21);
                let is_ram = gpa >= mem_base && gpa < mem_end;
                let flags = if is_ram {
                    PTE_V | PTE_R | PTE_W | PTE_X | PTE_A | PTE_D | PTE_U
                } else {
                    PTE_V | PTE_R | PTE_W | PTE_A | PTE_D
                };
                let pte = ((gpa >> 12) << 10) | flags;
                // SAFETY: l1_va is valid; j < 512.
                unsafe {
                    l1_va.add(j).write(pte);
                }
            }
        }

        log::info!(
            "[gstage] init mem={:#x}+{:#x} root_pa={:#x}",
            mem_base,
            mem_size,
            root_pa,
        );

        Some(Self { root_page, vmid })
    }

    fn map_region(&mut self, _gpa: u64, _hpa: u64, _size: u64, _perm: GuestPerm) -> bool {
        false
    }

    fn gpa_to_hpa(&self, gpa: u64) -> Option<u64> {
        if gpa < (L2_ENTRIES as u64) << 30 {
            Some(gpa)
        } else {
            None
        }
    }

    fn activate(&self) {
        let root_ppn = self.root_pa() >> 12;
        let hgatp = HGATP_MODE_SV39X4 | ((self.vmid as u64) << HGATP_VMID_SHIFT) | root_ppn;
        // SAFETY: writing hgatp CSR is safe from HS-mode.
        unsafe {
            core::arch::asm!(
                "csrw 0x680, {}",  // hgatp = CSR 0x680
                in(reg) hgatp,
            );
        }
        hfence_gvma();
    }

    fn unmap_range(&mut self, gpa: u64, size: u64) -> bool {
        let block_size: u64 = 1 << 21; // 2 MiB
        let start = gpa & !(block_size - 1);
        let end = (gpa + size + block_size - 1) & !(block_size - 1);

        let root_va = self.root_page.as_ptr() as *const u64;

        let mut addr = start;
        while addr < end {
            let root_idx = (addr >> 30) as usize;
            let l1_idx = ((addr >> 21) & 0x1FF) as usize;

            if root_idx >= L2_ENTRIES {
                return false;
            }

            // SAFETY: root_va is valid; root_idx < L2_ENTRIES.
            let root_pte = unsafe { root_va.add(root_idx).read() };
            if root_pte & PTE_V == 0 {
                addr += block_size;
                continue;
            }

            let l1_pa = (root_pte >> 10) << 12;
            let l1_va = kaddr_layout::p2v(l1_pa as usize) as *mut u64;
            // SAFETY: l1_va is valid; l1_idx < 512.
            unsafe {
                l1_va.add(l1_idx).write(0);
            }

            addr += block_size;
        }
        hfence_gvma();
        true
    }
}

fn hfence_gvma() {
    // SAFETY: hfence.gvma flushes G-stage TLB, safe from HS-mode.
    unsafe {
        core::arch::asm!("hfence.gvma", "sfence.vma");
    }
}

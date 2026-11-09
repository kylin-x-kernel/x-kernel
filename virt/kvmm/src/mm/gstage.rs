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
//! 2 TiB guest physical address range. Only the first four root entries
//! are populated, so the table covers `[0, 4 GiB)`.
//!
//! Mapping policy follows the AArch64 Stage-2 contract: nothing is mapped
//! unless explicitly requested. [`GStage::new`] maps the declared RAM range
//! with 2 MiB leaves and leaves every other entry INVALID, so a guest access
//! to an unbacked GPA traps instead of reaching host physical memory.
//! [`GStage::map_region`] then installs 4 KiB leaves on demand — splitting a
//! 2 MiB leaf into an L0 table when the request only covers part of it.

use super::{GuestMem, GuestPerm, free_pt_page};

// RISC-V PTE bits (same for S-stage and G-stage)
const PTE_V: u64 = 1 << 0;
const PTE_R: u64 = 1 << 1;
const PTE_W: u64 = 1 << 2;
const PTE_X: u64 = 1 << 3;
const PTE_U: u64 = 1 << 4;
const PTE_A: u64 = 1 << 6;
const PTE_D: u64 = 1 << 7;

/// Permission bits that describe a leaf; a set PTE is a table when none of
/// `R`/`W`/`X` is set.
const PTE_LEAF_PERM: u64 = PTE_V | PTE_R | PTE_W | PTE_X | PTE_U | PTE_A | PTE_D;

const L2_ENTRIES: usize = 4; // Root entries covering 4 GiB
const L1_ENTRIES: usize = 512; // Entries per L1 table (2 MiB each)
const L0_ENTRIES: usize = 512; // Entries per L0 table (4 KiB each)

const PAGE_SHIFT: u64 = 12;
const BLOCK_SHIFT: u64 = 21;
const PAGE_SIZE: u64 = 1 << PAGE_SHIFT;
const BLOCK_SIZE: u64 = 1 << BLOCK_SHIFT;

// hgatp mode field
const HGATP_MODE_SV39X4: u64 = 8 << 60;
const HGATP_VMID_SHIFT: u64 = 44;

/// Encode a PTE address field (PPN in bits 10..=53) from a physical address.
const fn pte_addr(pa: u64) -> u64 {
    (pa >> PAGE_SHIFT) << 10
}

/// Decode the physical address held in a PTE's address field.
const fn pte_pa(pte: u64) -> u64 {
    ((pte >> 10) & ((1u64 << 44) - 1)) << PAGE_SHIFT
}

/// A valid entry is a leaf when any of `R`/`W`/`X` is set, a table otherwise.
const fn is_leaf(pte: u64) -> bool {
    pte & PTE_V != 0 && pte & (PTE_R | PTE_W | PTE_X) != 0
}

const fn is_table(pte: u64) -> bool {
    pte & PTE_V != 0 && pte & (PTE_R | PTE_W | PTE_X) == 0
}

/// RISC-V G-stage page table.
///
/// Owns the root contiguous page (4 × 4 KiB = 16 KiB). On drop,
/// walks root entries to free L1 pages and any L0 tables they point
/// at, then the root page itself is freed by `PageRef::drop`.
pub struct GStage {
    root_page: kalloc::PageRef,
    vmid: u32,
}

impl GStage {
    /// Read entry `idx` of a live page table.
    ///
    /// # Safety
    ///
    /// `base` must point to an initialized page table with more than `idx`
    /// entries, and the caller must own that table.
    unsafe fn table_read(base: *const u64, idx: usize) -> u64 {
        // SAFETY: the caller guarantees `idx` is in bounds.
        let ptr = unsafe { base.add(idx) };
        // SAFETY: `ptr` is an aligned, initialized descriptor.
        unsafe { ptr.read() }
    }

    /// Write `value` to entry `idx` of a live page table.
    ///
    /// # Safety
    ///
    /// `base` must point to a page table with more than `idx` entries that the
    /// caller owns and may publish to hardware.
    unsafe fn table_write(base: *mut u64, idx: usize, value: u64) {
        // SAFETY: the caller guarantees `idx` is in bounds.
        let ptr = unsafe { base.add(idx) };
        // SAFETY: `ptr` is an aligned descriptor in a table we own.
        unsafe { ptr.write(value) };
    }

    fn root_pa(&self) -> u64 {
        self.root_page.phys_addr().as_usize() as u64
    }

    fn destroy(&mut self) {
        let root_va = kaddr_layout::p2v(self.root_page.phys_addr().as_usize()) as *const u64;
        for i in 0..L2_ENTRIES {
            // SAFETY: root_va points to 16 KiB; i < L2_ENTRIES < 2048.
            let root_pte = unsafe { Self::table_read(root_va, i) };
            if !is_table(root_pte) {
                continue;
            }

            let l1_pa = pte_pa(root_pte);
            let l1_va = kaddr_layout::p2v(l1_pa as usize) as *const u64;
            for j in 0..L1_ENTRIES {
                // SAFETY: l1_va was allocated by `new`; j < L1_ENTRIES.
                let l1_pte = unsafe { Self::table_read(l1_va, j) };
                if is_table(l1_pte) {
                    // SAFETY: L0 pages are only ever installed by
                    // `ensure_l0_table`, which allocates them via PageRef.
                    unsafe { free_pt_page(pte_pa(l1_pte)) };
                }
            }
            // SAFETY: l1_pa was allocated by `new` and is uniquely owned.
            unsafe { free_pt_page(l1_pa) };
        }
    }

    /// Ensure the L1 slot `(root_idx, l1_idx)` is an L0 table, returning its VA.
    ///
    /// Reuses an existing L0 table. Splits a 2 MiB leaf into 512 4 KiB leaves
    /// that preserve the block's translation and permissions, so only the
    /// pages the caller later overwrites change meaning. An already-invalid
    /// slot gets a zeroed (all-invalid) L0 table, which keeps trapping.
    ///
    /// Returns `None` on allocation failure or an out-of-range index. Callers
    /// are responsible for the `hfence.gvma` once their writes are complete.
    fn ensure_l0_table(&self, root_idx: usize, l1_idx: usize) -> Option<*mut u64> {
        let root_va = kaddr_layout::p2v(self.root_page.phys_addr().as_usize()) as *const u64;
        // SAFETY: root_va points to 16 KiB; root_idx < L2_ENTRIES < 2048.
        let root_pte = unsafe { Self::table_read(root_va, root_idx) };
        if !is_table(root_pte) {
            return None;
        }

        let l1_va = kaddr_layout::p2v(pte_pa(root_pte) as usize) as *mut u64;
        // SAFETY: l1_va was allocated by `new`; l1_idx < L1_ENTRIES.
        let l1_pte = unsafe { Self::table_read(l1_va, l1_idx) };
        if is_table(l1_pte) {
            return Some(kaddr_layout::p2v(pte_pa(l1_pte) as usize) as *mut u64);
        }

        let l0_page = kalloc::alloc_zeroed_page_ref(kalloc::UsageKind::Global).ok()?;
        let l0_pa = l0_page.phys_addr().as_usize() as u64;
        let l0_va = kaddr_layout::p2v(l0_pa as usize) as *mut u64;

        if is_leaf(l1_pte) {
            // 2 MiB leaf → 512 4 KiB leaves preserving the block's mapping.
            let block_perm = l1_pte & PTE_LEAF_PERM;
            let block_hpa = pte_pa(l1_pte);
            for i in 0..L0_ENTRIES {
                let page_hpa = block_hpa + (i as u64) * PAGE_SIZE;
                // SAFETY: l0_va is a freshly-allocated zeroed page; i < 512.
                unsafe { Self::table_write(l0_va, i, pte_addr(page_hpa) | block_perm) }
            }
        }
        // else: invalid L1 slot → leave the L0 all-invalid (region keeps trapping).

        // SAFETY: installing the L0 table into the L1 slot; l1_va is valid.
        unsafe { Self::table_write(l1_va, l1_idx, pte_addr(l0_pa) | PTE_V) }
        // Ownership now lives in the page table, so `l0_page` must not free it.
        let _ = l0_page.into_raw();

        Some(l0_va)
    }
}

impl Drop for GStage {
    fn drop(&mut self) {
        self.destroy();
    }
}

impl GuestMem for GStage {
    /// Build a G-stage table covering `[0, 4 GiB)`.
    ///
    /// Only `[mem_base, mem_base + mem_size)` is mapped, using 2 MiB leaves
    /// whose backing is `hpa_base`; every other entry is left invalid so the
    /// guest faults into the VMM. A zero-sized range maps nothing at all,
    /// which is what the lkvm path wants: all RAM arrives later through
    /// [`GStage::map_region`] from `KVM_SET_USER_MEMORY_REGION` memslots.
    ///
    /// A 2 MiB block is mapped only when it lies entirely inside the RAM
    /// range, so a misaligned `mem_base`/`mem_size` can never expose backing
    /// memory the caller did not declare. A caller that needs the partial
    /// blocks can install them with 4 KiB granularity via
    /// [`GStage::map_region`].
    fn new(mem_base: u64, mem_size: u64, hpa_base: u64, vmid: u32) -> Option<Self> {
        let mem_end = mem_base.checked_add(mem_size)?;

        // Sv39x4 root: 4 contiguous 4KB pages (16KB, 16KB-aligned), zeroed.
        let root_page =
            kalloc::alloc_zeroed_pages_exact_ref(4, 4 * 4096, kalloc::UsageKind::Global).ok()?;
        let root_pa = root_page.phys_addr().as_usize() as u64;
        let root_va = kaddr_layout::p2v(root_pa as usize) as *mut u64;

        // Allocate 4 L1 tables and fill root entries
        for i in 0..L2_ENTRIES {
            let l1_page = kalloc::alloc_zeroed_page_ref(kalloc::UsageKind::Global).ok()?;
            let l1_pa = l1_page.phys_addr().as_usize() as u64;
            let l1_va = kaddr_layout::p2v(l1_pa as usize) as *mut u64;
            // Ownership moves into the G-stage table until `destroy`.
            let _ = l1_page.into_raw();

            // Root entry: non-leaf (V=1, R=W=X=0) pointing to L1
            // SAFETY: root_va is valid; i < L2_ENTRIES < 2048.
            unsafe { Self::table_write(root_va, i, pte_addr(l1_pa) | PTE_V) }

            // Fill L1 with 2 MiB superpage entries. Non-RAM (and any partial
            // block at either end of the range) stays invalid so guest
            // accesses trap instead of aliasing host memory.
            for j in 0..L1_ENTRIES {
                let gpa = ((i as u64) << 30) | ((j as u64) << BLOCK_SHIFT);
                let block_end = gpa + BLOCK_SIZE;
                if gpa < mem_base || block_end > mem_end {
                    continue;
                }
                let hpa = hpa_base + (gpa - mem_base);
                let pte = pte_addr(hpa) | PTE_V | PTE_R | PTE_W | PTE_X | PTE_A | PTE_D | PTE_U;
                // SAFETY: l1_va is valid; j < L1_ENTRIES.
                unsafe { Self::table_write(l1_va, j, pte) }
            }
        }

        log::info!(
            "[gstage] init mem={:#x}+{:#x} hpa={:#x} root_pa={:#x}",
            mem_base,
            mem_size,
            hpa_base,
            root_pa,
        );

        Some(Self { root_page, vmid })
    }

    /// Map `[gpa, gpa + size)` onto `[hpa, hpa + size)` at 4 KiB granularity.
    ///
    /// All three parameters must be 4 KiB aligned. Requests are never rounded
    /// outward: doing so would map backing pages the caller did not offer.
    fn map_region(&self, gpa: u64, hpa: u64, size: u64, perm: GuestPerm) -> bool {
        if size == 0 {
            return true;
        }
        if !gpa.is_multiple_of(PAGE_SIZE)
            || !hpa.is_multiple_of(PAGE_SIZE)
            || !size.is_multiple_of(PAGE_SIZE)
        {
            log::error!(
                "[gstage] map_region rejected unaligned gpa={:#x} hpa={:#x} size={:#x}",
                gpa,
                hpa,
                size,
            );
            return false;
        }

        let attr = match perm {
            // `U` is required: a Linux guest touches its own RAM from
            // userspace, and a G-stage leaf without it faults VU-mode.
            GuestPerm::RamRw => PTE_V | PTE_R | PTE_W | PTE_X | PTE_U | PTE_A | PTE_D,
            // Device windows are never accessible from guest userspace and
            // are never executed from.
            GuestPerm::DeviceRw => PTE_V | PTE_R | PTE_W | PTE_A | PTE_D,
        };

        let npages = size / PAGE_SIZE;
        for n in 0..npages {
            let cur_gpa = gpa + n * PAGE_SIZE;
            let cur_hpa = hpa + n * PAGE_SIZE;

            let root_idx = (cur_gpa >> 30) as usize;
            let l1_idx = ((cur_gpa >> BLOCK_SHIFT) & 0x1FF) as usize;
            let l0_idx = ((cur_gpa >> PAGE_SHIFT) & 0x1FF) as usize;

            if root_idx >= L2_ENTRIES {
                return false;
            }

            let Some(l0_va) = self.ensure_l0_table(root_idx, l1_idx) else {
                return false;
            };

            // SAFETY: l0_va points to a valid L0 page; l0_idx < L0_ENTRIES.
            unsafe { Self::table_write(l0_va, l0_idx, pte_addr(cur_hpa) | attr) }
        }

        hfence_gvma();
        true
    }

    /// Translate `gpa` by walking the G-stage tables.
    ///
    /// Walks `root → L1 → (L0)` and accepts either a 2 MiB leaf or a 4 KiB
    /// leaf. Returns `None` for any invalid entry, so an unmapped GPA reports
    /// as unmapped rather than aliasing host memory.
    fn gpa_to_hpa(&self, gpa: u64) -> Option<u64> {
        let root_idx = (gpa >> 30) as usize;
        if root_idx >= L2_ENTRIES {
            return None;
        }

        let root_va = kaddr_layout::p2v(self.root_page.phys_addr().as_usize()) as *const u64;
        // SAFETY: root_va points to 16 KiB; root_idx < L2_ENTRIES.
        let root_pte = unsafe { Self::table_read(root_va, root_idx) };
        if !is_table(root_pte) {
            return None;
        }

        let l1_idx = ((gpa >> BLOCK_SHIFT) & 0x1FF) as usize;
        let l1_va = kaddr_layout::p2v(pte_pa(root_pte) as usize) as *const u64;
        // SAFETY: l1_va was allocated by `new`; l1_idx < L1_ENTRIES.
        let l1_pte = unsafe { Self::table_read(l1_va, l1_idx) };

        if is_leaf(l1_pte) {
            // 2 MiB superpage.
            return Some(pte_pa(l1_pte) | (gpa & (BLOCK_SIZE - 1)));
        }
        if !is_table(l1_pte) {
            return None;
        }

        let l0_idx = ((gpa >> PAGE_SHIFT) & 0x1FF) as usize;
        let l0_va = kaddr_layout::p2v(pte_pa(l1_pte) as usize) as *const u64;
        // SAFETY: l0_va was installed by `ensure_l0_table`; l0_idx < 512.
        let l0_pte = unsafe { Self::table_read(l0_va, l0_idx) };
        if !is_leaf(l0_pte) {
            return None;
        }

        Some(pte_pa(l0_pte) | (gpa & (PAGE_SIZE - 1)))
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

    /// Remove mappings for `[gpa, gpa + size)` so guest accesses trap again.
    ///
    /// The range is rounded outward to 2 MiB to find the affected L1 slots.
    /// A slot holding a 2 MiB leaf is cleared whole; a slot holding an L0
    /// table has only the intersecting 4 KiB leaves cleared, and the table is
    /// freed once every leaf in it is invalid. Ranges that are already
    /// unmapped are a successful no-op — callers treat `false` as fatal and
    /// rely on that to punch holes that were never mapped in the first place.
    fn unmap_range(&self, gpa: u64, size: u64) -> bool {
        if size == 0 {
            return true;
        }

        let start = gpa & !(BLOCK_SIZE - 1);
        let end = (gpa + size).div_ceil(BLOCK_SIZE) * BLOCK_SIZE;
        let root_va = kaddr_layout::p2v(self.root_page.phys_addr().as_usize()) as *const u64;

        let mut addr = start;
        while addr < end {
            let root_idx = (addr >> 30) as usize;
            let l1_idx = ((addr >> BLOCK_SHIFT) & 0x1FF) as usize;

            if root_idx >= L2_ENTRIES {
                return false;
            }

            // SAFETY: root_va is valid; root_idx < L2_ENTRIES.
            let root_pte = unsafe { Self::table_read(root_va, root_idx) };
            if !is_table(root_pte) {
                addr += BLOCK_SIZE;
                continue;
            }

            let l1_va = kaddr_layout::p2v(pte_pa(root_pte) as usize) as *mut u64;
            // SAFETY: l1_va was allocated by `new`; l1_idx < L1_ENTRIES.
            let l1_pte = unsafe { Self::table_read(l1_va, l1_idx) };
            if l1_pte & PTE_V == 0 {
                addr += BLOCK_SIZE;
                continue;
            }

            if is_leaf(l1_pte) {
                // 2 MiB superpage: a sub-block request still drops the whole
                // leaf, because the G-stage cannot represent the remainder.
                // SAFETY: l1_va is valid; l1_idx < L1_ENTRIES.
                unsafe { Self::table_write(l1_va, l1_idx, 0) }
            } else {
                let l0_va = kaddr_layout::p2v(pte_pa(l1_pte) as usize) as *mut u64;
                let block_start = addr;
                let from = gpa.max(block_start);
                let to = (gpa + size).min(block_start + BLOCK_SIZE);
                let first = ((from - block_start) >> PAGE_SHIFT) as usize;
                let last = (to - block_start).div_ceil(PAGE_SIZE) as usize;

                let mut remaining = 0;
                for i in 0..L0_ENTRIES {
                    // SAFETY: l0_va was installed by `ensure_l0_table`; i < 512.
                    let pte = unsafe { Self::table_read(l0_va, i) };
                    let in_range = i >= first && i < last;
                    if in_range && pte & PTE_V != 0 {
                        // SAFETY: as above.
                        unsafe { Self::table_write(l0_va, i, 0) }
                    } else if !in_range && pte & PTE_V != 0 {
                        remaining += 1;
                    }
                }

                if remaining == 0 {
                    // Nothing left in this 2 MiB block: drop the L0 table too
                    // so a fully unmapped region does not leak page-table pages.
                    // SAFETY: l0_va was installed by `ensure_l0_table`.
                    // SAFETY: l1_va is valid; l1_idx < L1_ENTRIES.
                    unsafe { Self::table_write(l1_va, l1_idx, 0) };
                    // SAFETY: l1_pte referenced an L0 table allocated by
                    // `ensure_l0_table`, now unreachable from the table.
                    unsafe { free_pt_page(pte_pa(l1_pte)) };
                }
            }

            addr += BLOCK_SIZE;
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

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Guest physical memory management: second-stage page tables and MMIO bus.

use core::sync::atomic::{AtomicU32, Ordering};

static NEXT_VMID: AtomicU32 = AtomicU32::new(1);
const PAGE_SIZE_4K: usize = 4096;

/// Allocate a unique VMID for a new VM.
pub fn alloc_vmid() -> u32 {
    NEXT_VMID.fetch_add(1, Ordering::Relaxed)
}

/// Reserve an identity-mapped guest RAM range in the host allocator.
///
/// Current second-stage implementations map guest RAM as GPA == HPA. Reserving
/// the backing physical pages prevents later host allocations from reusing guest
/// RAM and also validates that the alias is backed by host RAM before the loader
/// writes through it.
pub fn reserve_guest_ram(mem_base: u64, mem_size: u64) -> bool {
    let npages = (mem_size as usize) / PAGE_SIZE_4K;
    let va = kaddr_layout::p2v(mem_base as usize);

    match kalloc::global_allocator().alloc_pages_at(
        va,
        npages,
        PAGE_SIZE_4K,
        kalloc::UsageKind::VirtMem,
    ) {
        Ok(_) => {
            log::info!(
                "[kvmm] reserved guest RAM GPA {:#x}+{:#x} ({} pages)",
                mem_base,
                mem_size,
                npages,
            );
            true
        }
        Err(err) => {
            log::error!(
                "[kvmm] reserve guest RAM {:#x}+{:#x} failed: {:?}",
                mem_base,
                mem_size,
                err,
            );
            false
        }
    }
}

#[cfg(target_arch = "aarch64")]
pub mod stage2;

#[cfg(target_arch = "x86_64")]
pub mod ept;

#[cfg(target_arch = "riscv64")]
pub mod gstage;

pub mod mmio;

/// Free a page-table page given its physical address.
///
/// # Safety
///
/// `pa` must be the physical address of a page originally allocated by
/// `GlobalPage::alloc*` and not currently owned by another `GlobalPage`.
#[cfg(any(
    target_arch = "aarch64",
    target_arch = "x86_64",
    target_arch = "riscv64"
))]
pub(crate) unsafe fn free_pt_page(pa: u64) {
    let va = kaddr_layout::p2v(pa as usize);
    // SAFETY: caller guarantees `pa` came from GlobalPage::alloc and is uniquely owned.
    drop(unsafe { kalloc::GlobalPage::from_raw(va.into(), 1) });
}

/// Guest memory permission attributes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestPerm {
    /// Normal cacheable read/write (RAM).
    RamRW,
    /// Device memory read/write (MMIO regions).
    DeviceRW,
}

/// Second-stage address translation (GPA → HPA).
///
/// Each architecture provides an implementation that programs the
/// hardware page table registers (VTTBR_EL2, EPTP, hgatp).
pub trait GuestMem: Sized {
    /// Build an identity-mapped page table covering `[0, 4 GiB)`.
    ///
    /// RAM in `[mem_base, mem_base+mem_size)` gets normal cacheable
    /// attributes; everything else gets device attributes.
    ///
    /// Returns `None` if page table allocation fails.
    fn new(mem_base: u64, mem_size: u64, vmid: u32) -> Option<Self>;

    /// Map a region of guest physical address space.
    fn map_region(&mut self, gpa: u64, hpa: u64, size: u64, perm: GuestPerm) -> bool;

    /// Translate a guest physical address to a host physical address.
    fn gpa_to_hpa(&self, gpa: u64) -> Option<u64>;

    /// Write the page table root into the hardware register so the
    /// second-stage translation takes effect for subsequent guest entries.
    fn activate(&self);

    /// Remove mappings for the given GPA range so that guest accesses trap.
    ///
    /// The range is rounded outward to the page table granularity (e.g. 2 MiB
    /// blocks on AArch64 Stage-2). Returns `true` on success.
    fn unmap_range(&mut self, _gpa: u64, _size: u64) -> bool {
        false
    }
}

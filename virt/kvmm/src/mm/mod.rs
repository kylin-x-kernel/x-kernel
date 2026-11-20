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

/// Owned lifetime protection for allocator-chosen guest RAM backing pages.
///
/// The VM must retain this reservation through all guest accesses and required
/// translation retirement. Dropping it releases its exact independent page
/// holds; other `PageRef` holders can keep the backing alive.
pub struct GuestRamReservation {
    backing: kalloc::PageRef,
}

impl GuestRamReservation {
    /// Return the beginning of the contiguous host physical backing range.
    pub fn hpa_base(&self) -> u64 {
        self.backing.phys_addr().as_usize() as u64
    }
}

/// Reserve zeroed backing memory for a guest RAM range.
///
/// The guest RAM range starts at `mem_base` in the guest physical address space.
/// The backing host physical range is allocator-chosen and may differ from the
/// guest base; [`GuestMem::gpa_to_hpa`] translates between them.
///
/// Returns `None` for a zero or non-page-aligned size, an overflowing guest
/// range, a size not representable on the host, or an allocation failure.
/// Allocation and final release require initialized host page metadata and
/// direct mapping. VM control-plane callers must retain the reservation until
/// all guest accesses and required translation retirement have completed.
pub fn reserve_guest_ram(mem_base: u64, mem_size: u64) -> Option<GuestRamReservation> {
    mem_base.checked_add(mem_size)?;
    let size_bytes = usize::try_from(mem_size).ok()?;
    if size_bytes == 0 || !size_bytes.is_multiple_of(PAGE_SIZE_4K) {
        return None;
    }
    let npages = size_bytes / PAGE_SIZE_4K;
    match kalloc::alloc_zeroed_pages_exact_ref(npages, PAGE_SIZE_4K, kalloc::UsageKind::VirtMem) {
        Ok(backing) => {
            log::info!(
                "[kvmm] reserved guest RAM GPA {:#x}+{:#x} backed by HPA {:#x} ({} pages)",
                mem_base,
                mem_size,
                backing.phys_addr().as_usize(),
                npages,
            );
            Some(GuestRamReservation { backing })
        }
        Err(err) => {
            log::error!(
                "[kvmm] reserve guest RAM {:#x}+{:#x} failed: {:?}",
                mem_base,
                mem_size,
                err,
            );
            None
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

/// Supply fresh zeroed exact backing to the private table-allocation callbacks.
#[cfg(any(
    target_arch = "aarch64",
    target_arch = "x86_64",
    target_arch = "riscv64"
))]
fn alloc_table_pages(page_count: usize, alignment_bytes: usize) -> Option<kalloc::PageRef> {
    kalloc::alloc_zeroed_pages_exact_ref(page_count, alignment_bytes, kalloc::UsageKind::Global)
        .ok()
}

/// Free a page-table page given its physical address.
///
/// # Safety
///
/// `pa` must identify one outstanding PageRef::into_raw transfer with length one
/// from the same allocation. End the table's hardware use and required
/// translation retirement before consuming this transfer.
#[cfg(any(
    target_arch = "aarch64",
    target_arch = "x86_64",
    target_arch = "riscv64"
))]
pub(crate) unsafe fn free_pt_page(pa: u64) {
    // SAFETY: the caller consumes this exact outstanding table-page transfer
    // once after ending hardware use and required translation retirement.
    drop(unsafe { kalloc::PageRef::from_raw((pa as usize).into(), 1) });
}

/// Guest memory permission attributes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestPerm {
    /// Normal cacheable read/write (RAM).
    RamRw,
    /// Device memory read/write (MMIO regions).
    DeviceRw,
}

/// Second-stage address translation (GPA → HPA).
///
/// Each architecture provides an implementation that programs the
/// hardware page table registers (VTTBR_EL2, EPTP, hgatp).
pub trait GuestMem: Sized {
    /// Build an identity-mapped page table covering `[0, 4 GiB)`.
    ///
    /// RAM in `[mem_base, mem_base+mem_size)` maps to the backing host physical
    /// range starting at `hpa_base`; everything else gets device attributes or
    /// traps according to the architecture implementation.
    ///
    /// Returns `None` if the guest range overflows or page table allocation
    /// fails. Failed construction releases the entire unpublished table prefix;
    /// no root or child table remains owned by the failed attempt.
    fn new(mem_base: u64, mem_size: u64, hpa_base: u64, vmid: u32) -> Option<Self>;

    /// Map a region of guest physical address space.
    ///
    /// This takes `&self` because implementations update the page-table pages
    /// they own through raw pointers; callers must serialize it with
    /// [`unmap_range`](Self::unmap_range) and with software table walks. Use
    /// `Vm::map_guest_region` rather than calling this directly.
    fn map_region(&self, gpa: u64, hpa: u64, size: u64, perm: GuestPerm) -> bool;

    /// Translate a guest physical address to a host physical address.
    fn gpa_to_hpa(&self, gpa: u64) -> Option<u64>;

    /// Write the page table root into the hardware register so the
    /// second-stage translation takes effect for subsequent guest entries.
    ///
    /// Only immutable per-VM state (root table address, VMID) is read, so this
    /// may run concurrently with `map_region`/`unmap_range`.
    fn activate(&self);

    /// Remove mappings for the given GPA range so that guest accesses trap.
    ///
    /// The range is rounded outward to 4 KiB pages, but never to a larger
    /// page-table block: partial ranges are handled at the smallest supported
    /// granularity. Entries that are already invalid are treated as success.
    /// Returns `true` on success.
    ///
    /// Like [`map_region`](Self::map_region), callers must serialize this with
    /// other table mutations; use `Vm::unmap_guest_region`.
    fn unmap_range(&self, _gpa: u64, _size: u64) -> bool {
        false
    }
}

#[cfg(any(
    target_arch = "aarch64",
    target_arch = "x86_64",
    target_arch = "riscv64"
))]
unittest::path_mod_test!(tests, "../../ut-tests/guest_memory.rs");

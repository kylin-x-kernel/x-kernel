//! [ArceOS](https://github.com/arceos-org/arceos) memory management module.
#![no_std]
#[macro_use]
extern crate log;
extern crate alloc;
mod aspace;
pub mod backend;

pub use self::aspace::AddrSpace;
pub use self::backend::Backend;

use axerrno::AxResult;
use axhal::mem::{MemRegionFlags, phys_to_virt};
use axhal::paging::MappingFlags;
use kspin::SpinNoIrq;
use lazyinit::LazyInit;
use memory_addr::{MemoryAddr, PhysAddr, VirtAddr, pa, va};

static KERNEL_ASPACE: LazyInit<SpinNoIrq<AddrSpace>> = LazyInit::new();

fn reg_flag_to_map_flag(f: MemRegionFlags) -> MappingFlags {
    let mut ret = MappingFlags::empty();
    if f.contains(MemRegionFlags::READ) {
        ret |= MappingFlags::READ;
    }
    if f.contains(MemRegionFlags::WRITE) {
        ret |= MappingFlags::WRITE;
    }
    if f.contains(MemRegionFlags::EXECUTE) {
        ret |= MappingFlags::EXECUTE;
    }
    if f.contains(MemRegionFlags::DEVICE) {
        ret |= MappingFlags::DEVICE;
    }
    if f.contains(MemRegionFlags::UNCACHED) {
        ret |= MappingFlags::UNCACHED;
    }
    ret
}

/// Creates a new address space for kernel itself.
pub fn new_kernel_aspace() -> AxResult<AddrSpace> {
    let cbit_mask = sev_cbit_mask();
    let shared_range = sev_shared_memory_range();
    let mut aspace = AddrSpace::new_empty(
        va!(axconfig::plat::KERNEL_ASPACE_BASE),
        axconfig::plat::KERNEL_ASPACE_SIZE,
    )?;
    for r in axhal::mem::memory_regions() {
        // mapped range should contain the whole region if it is not aligned.
        let start = r.paddr.align_down_4k();
        let end = (r.paddr + r.size).align_up_4k();

        // For SEV, we need to handle shared memory regions specially
        if cbit_mask != 0
            && !r.flags.contains(MemRegionFlags::DEVICE)
            && !r.flags.contains(MemRegionFlags::UNCACHED)
        {
            // Split the region if it overlaps with shared memory
            map_region_with_shared_split(
                &mut aspace,
                start,
                end,
                cbit_mask,
                shared_range,
                reg_flag_to_map_flag(r.flags),
            )?;
        } else {
            // DEVICE or UNCACHED regions are mapped without C-Bit
            aspace.map_linear(
                phys_to_virt(start),
                start,
                end - start,
                reg_flag_to_map_flag(r.flags),
            )?;
        }
    }
    Ok(aspace)
}

/// Maps a memory region, splitting it if necessary to handle shared memory.
/// 
/// For AMD SEV, shared memory (used for VirtIO DMA) must be mapped without
/// the C-Bit, while normal memory must have the C-Bit set.
fn map_region_with_shared_split(
    aspace: &mut AddrSpace,
    start: PhysAddr,
    end: PhysAddr,
    _cbit_mask: usize,
    shared_range: (usize, usize),
    flags: MappingFlags,
) -> AxResult<()> {
    let start_usize = start.as_usize();
    let end_usize = end.as_usize();
    let (shared_start, shared_end) = shared_range;

    // If no shared range or no overlap, map the entire region (encrypted)
    if shared_start == 0 && shared_end == 0 {
        return aspace.map_linear(phys_to_virt(start), start, end - start, flags);
    }

    // Check for overlap
    if start_usize >= shared_end || end_usize <= shared_start {
        // No overlap, map normally (encrypted)
        return aspace.map_linear(phys_to_virt(start), start, end - start, flags);
    }

    // There is overlap, need to split the region
    // Region 1: [start, shared_start) - encrypted (normal flags)
    if start_usize < shared_start {
        let region_end = shared_start.min(end_usize);
        aspace.map_linear(
            phys_to_virt(start),
            start,
            region_end - start_usize,
            flags,
        )?;
    }

    // Region 2: [max(start, shared_start), min(end, shared_end)) - shared (with SHARED flag)
    let overlap_start = start_usize.max(shared_start);
    let overlap_end = end_usize.min(shared_end);
    if overlap_start < overlap_end {
        let paddr = pa!(overlap_start);
        // Add SHARED flag to indicate this memory should not be encrypted
        aspace.map_linear(
            phys_to_virt(paddr),
            paddr,
            overlap_end - overlap_start,
            flags | MappingFlags::SHARED,
        )?;
    }

    // Region 3: [shared_end, end) - encrypted (normal flags)
    if end_usize > shared_end && shared_end > start_usize {
        let region_start = shared_end.max(start_usize);
        let paddr = pa!(region_start);
        aspace.map_linear(
            phys_to_virt(paddr),
            paddr,
            end_usize - region_start,
            flags,
        )?;
    }

    Ok(())
}

/// Creates a new address space for user processes.
pub fn new_user_aspace(base: VirtAddr, size: usize) -> AxResult<AddrSpace> {
    let mut aspace = AddrSpace::new_empty(base, size)?;
    if !cfg!(target_arch = "aarch64") && !cfg!(target_arch = "loongarch64") {
        // ARMv8 (aarch64) and LoongArch64 use separate page tables for user space
        // (aarch64: TTBR0_EL1, LoongArch64: PGDL), so there is no need to copy the
        // kernel portion to the user page table.
        aspace.copy_mappings_from(&kernel_aspace().lock())?;
    }
    Ok(aspace)
}

/// Returns the globally unique kernel address space.
pub fn kernel_aspace() -> &'static SpinNoIrq<AddrSpace> {
    &KERNEL_ASPACE
}
/// Returns the root physical address of the kernel page table.
pub fn kernel_page_table_root() -> PhysAddr {
    KERNEL_ASPACE.lock().page_table_root()
}
/// Initializes virtual memory management.
///
/// It mainly sets up the kernel virtual memory address space and recreate a
/// fine-grained kernel page table.
pub fn init_memory_management() {
    info!("Initialize virtual memory management...");

    // Initialize SEV C-Bit in page table entry module for AMD SEV platforms
    #[cfg(target_arch = "x86_64")]
    {
        let cbit_pos = sev_cbit_pos();
        debug!("SEV C-Bit position = {}", cbit_pos);
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        if cbit_pos > 0 {
            page_table_multiarch::x86_64::init_sev_cbit(cbit_pos);
            // Ensure the C-Bit initialization is visible before creating page tables
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
            debug!("SEV C-Bit initialized: mask = {:#x}", 1usize << cbit_pos);
        }
    }

    let kernel_aspace = new_kernel_aspace().expect("failed to initialize kernel address space");
    debug!("kernel address space init OK: {:#x?}", kernel_aspace);
    KERNEL_ASPACE.init_once(SpinNoIrq::new(kernel_aspace));
    let mut root = kernel_page_table_root();
    let cbit_mask = sev_cbit_mask();
    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
    debug!("SEV C-Bit mask = {:#x}, ROOT = {:#x}", cbit_mask, root);
    if cbit_mask != 0 {
        root = PhysAddr::from(root.as_usize() | cbit_mask);
    }
    unsafe { axhal::asm::write_kernel_page_table(root) };
}

/// Initializes kernel paging for secondary CPUs.
pub fn init_memory_management_secondary() {
    let mut root = kernel_page_table_root();
    let cbit_mask = sev_cbit_mask();
    if cbit_mask != 0 {
        root = PhysAddr::from(root.as_usize() | cbit_mask);
    }
    unsafe { axhal::asm::write_kernel_page_table(root) };
}

/// Returns the SEV C-Bit position (0 if SEV is not enabled).
#[cfg(target_arch = "x86_64")]
#[inline]
fn sev_cbit_pos() -> u8 {
    // SAFETY: CPUID is available on x86_64.
    let max = unsafe { core::arch::x86_64::__cpuid_count(0x8000_0000, 0) }.eax;
    if max < 0x8000_001f {
        return 0;
    }
    let r = unsafe { core::arch::x86_64::__cpuid_count(0x8000_001f, 0) };
    if (r.eax & (1 << 1)) == 0 {
        return 0;
    }
    (r.ebx & 0x3f) as u8
}

#[inline]
fn sev_cbit_mask() -> usize {
    #[cfg(target_arch = "x86_64")]
    {
        let cbit_pos = sev_cbit_pos() as usize;
        if cbit_pos == 0 { 0 } else { 1usize << cbit_pos }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        0
    }
}

/// Returns the shared memory range for AMD SEV VirtIO DMA buffers.
///
/// This memory region is mapped without the C-Bit, making it accessible
/// to both guest and host for VirtIO device communication.
#[inline]
fn sev_shared_memory_range() -> (usize, usize) {
    #[cfg(target_arch = "x86_64")]
    {
        // Check if SEV is enabled
        if sev_cbit_mask() == 0 {
            return (0, 0);
        }
        // Try to get shared memory range from platform config
        #[cfg(any())]
        {
            // When platform provides these configs
            (
                axconfig::plat::SHARED_MEM_BASE,
                axconfig::plat::SHARED_MEM_BASE + axconfig::plat::SHARED_MEM_SIZE,
            )
        }
        #[cfg(not(any()))]
        {
            // Default shared memory region: 16MB base, 2MB size
            const DEFAULT_SHARED_MEM_BASE: usize = 0x0100_0000;
            const DEFAULT_SHARED_MEM_SIZE: usize = 0x0020_0000;
            (
                DEFAULT_SHARED_MEM_BASE,
                DEFAULT_SHARED_MEM_BASE + DEFAULT_SHARED_MEM_SIZE,
            )
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        (0, 0)
    }
}
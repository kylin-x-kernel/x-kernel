// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Virtual address space management utilities.
#![no_std]

#[macro_use]
extern crate log;

extern crate alloc;

#[cfg(target_arch = "aarch64")]
mod aarch64_asid;
mod aspace;
pub mod backend;
mod fault;
mod iomap;
mod vma;

use kaddr_layout::{KERNEL_ASPACE_BASE, KERNEL_ASPACE_SIZE};
use kerrno::LinuxResult;
use khal::{
    mem::{MemFlags, MemoryRegion, memory_regions, p2v},
    paging::MappingFlags,
};
use kspin::SpinNoIrq;
use lazyinit::LazyInit;
use memaddr::{MemoryAddr, PhysAddr, va};
pub use vmobj::{ObjectInvalidateRequest, VmObjectId};

#[cfg(target_arch = "aarch64")]
pub use self::aarch64_asid::Aarch64UserAsidContext;
pub use self::{
    aspace::{AddrPolicy, AddrSpace, InvalidateHandle, MmSpace, MremapSource},
    fault::{FaultContext, FaultInput, FaultOutcome, PageFaultOutcome},
    iomap::{
        DeviceRegion, DeviceRegionIter, IoMapError, device_regions, iomap_device, iounmap,
        register_device_region, register_fixed_device_region,
    },
    vma::{
        FileMappingInfo, ForkCloneTarget, MsyncPolicy, MsyncRuntimeResult, VmArea, VmAreaSet,
        VmBackingInfo, VmBackingKind, VmInheritance, VmMayPerm, VmPerm, VmRuntimeOps, VmRuntimeRef,
    },
};

static KERNEL_ASPACE: LazyInit<SpinNoIrq<MmSpace>> = LazyInit::new();

fn mem_to_mapping_flags(f: MemFlags) -> MappingFlags {
    let mut flags = MappingFlags::empty();

    let mappings = [
        (MemFlags::READ, MappingFlags::READ),
        (MemFlags::WRITE, MappingFlags::WRITE),
        (MemFlags::EXECUTE, MappingFlags::EXECUTE),
        (MemFlags::DEVICE, MappingFlags::DEVICE),
        (MemFlags::UNCACHED, MappingFlags::UNCACHED),
    ];

    for (mem_flag, map_flag) in mappings.iter() {
        if f.contains(*mem_flag) {
            flags |= *map_flag;
        }
    }

    flags
}

fn map_memory_region(vmspace: &mut MmSpace, region: MemoryRegion) -> LinuxResult<()> {
    let start = region.paddr.align_down_4k();
    let end = (region.paddr + region.size).align_up_4k();
    let target_va = match region.vaddr {
        Some(v) => v.align_down_4k(),
        None => p2v(start),
    };
    vmspace.map_linear(
        target_va,
        start,
        end - start,
        mem_to_mapping_flags(region.flags),
    )?;
    Ok(())
}

/// Creates a new address space for kernel itself.
pub fn new_kernel_layout() -> LinuxResult<MmSpace> {
    let mut vmspace =
        MmSpace::new_empty_kernel(va!(KERNEL_ASPACE_BASE as _), KERNEL_ASPACE_SIZE as _)?;
    for region in memory_regions() {
        map_memory_region(&mut vmspace, region)?;
    }
    for region in device_regions() {
        let mut mapped = MemoryRegion::new_mmio(region.paddr.as_usize(), region.size, region.name);
        mapped.vaddr = region.vaddr;
        map_memory_region(&mut vmspace, mapped)?;
    }
    Ok(vmspace)
}

/// Returns the globally unique kernel address space.
pub fn kernel_layout() -> &'static SpinNoIrq<MmSpace> {
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
    assert!(
        kalloc::is_page_allocator_ready(),
        "kernel page allocator must be initialized before init_memory_management()"
    );
    let kernel_layout = new_kernel_layout().expect("failed to initialize kernel address space");
    debug!("kernel address space init OK: {:#x?}", kernel_layout);
    KERNEL_ASPACE.init_once(SpinNoIrq::new(kernel_layout));
    // SAFETY: `kernel_page_table_root()` returns the initialized kernel root
    // page table that must become active before continuing boot.
    unsafe { karch::write_kernel_page_table(kernel_page_table_root().into()) };
    // flush all TLB
    karch::flush_tlb(None);
}

/// Initializes kernel paging for secondary CPUs.
pub fn init_memory_management_secondary() {
    assert!(
        KERNEL_ASPACE.get().is_some(),
        "kernel address space must be initialized before secondary MMU activation"
    );
    // SAFETY: secondary CPUs switch to the already initialized kernel root page table.
    unsafe { karch::write_kernel_page_table(kernel_page_table_root().into()) };
    // flush all TLB
    karch::flush_tlb(None);
}

#[cfg(unittest)]
mod tests_memspace {
    use khal::{mem::MemFlags, paging::MappingFlags};
    use unittest::def_test;

    use super::mem_to_mapping_flags;

    #[def_test]
    fn test_mem_to_mapping_flags_basic() {
        let flags = MemFlags::READ | MemFlags::WRITE;
        let mapped = mem_to_mapping_flags(flags);
        assert!(mapped.contains(MappingFlags::READ));
        assert!(mapped.contains(MappingFlags::WRITE));
    }

    #[def_test]
    fn test_mem_to_mapping_flags_device_uncached() {
        let flags = MemFlags::DEVICE | MemFlags::UNCACHED;
        let mapped = mem_to_mapping_flags(flags);
        assert!(mapped.contains(MappingFlags::DEVICE));
        assert!(mapped.contains(MappingFlags::UNCACHED));
    }

    #[def_test]
    fn test_mem_to_mapping_flags_empty() {
        let mapped = mem_to_mapping_flags(MemFlags::empty());
        assert!(mapped.is_empty());
    }
}

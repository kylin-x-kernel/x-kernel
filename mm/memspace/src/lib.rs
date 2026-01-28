#![no_std]

#[macro_use]
extern crate log;

extern crate alloc;

mod aspace;
pub mod backend;

use axerrno::LinuxResult;
use khal::{
    mem::{MemFlags, memory_regions, p2v},
    paging::MappingFlags,
};
use kspin::SpinNoIrq;
use lazyinit::LazyInit;
use memaddr::{MemoryAddr, PhysAddr, va};

pub use self::aspace::AddrSpace;

static KERNEL_ASPACE: LazyInit<SpinNoIrq<AddrSpace>> = LazyInit::new();

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

/// Creates a new address space for kernel itself.
pub fn new_kernel_layout() -> LinuxResult<AddrSpace> {
    let mut vmspace = AddrSpace::new_empty(
        va!(platconfig::plat::KERNEL_ASPACE_BASE),
        platconfig::plat::KERNEL_ASPACE_SIZE,
    )?;
    for region in memory_regions() {
        // mapped range should contain the whole region if it is not aligned.
        let start = region.paddr.align_down_4k();
        let end = (region.paddr + region.size).align_up_4k();
        vmspace.map_linear(
            p2v(start),
            start,
            end - start,
            mem_to_mapping_flags(region.flags),
        )?;
    }
    Ok(vmspace)
}

/// Returns the globally unique kernel address space.
pub fn kernel_layout() -> &'static SpinNoIrq<AddrSpace> {
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

    let kernel_layout = new_kernel_layout().expect("failed to initialize kernel address space");
    debug!("kernel address space init OK: {:#x?}", kernel_layout);
    KERNEL_ASPACE.init_once(SpinNoIrq::new(kernel_layout));
    unsafe { khal::asm::write_kernel_page_table(kernel_page_table_root()) };
    // flush all TLB
    khal::asm::flush_tlb(None);
}

/// Initializes kernel paging for secondary CPUs.
pub fn init_memory_management_secondary() {
    unsafe { khal::asm::write_kernel_page_table(kernel_page_table_root()) };
    // flush all TLB
    khal::asm::flush_tlb(None);
}

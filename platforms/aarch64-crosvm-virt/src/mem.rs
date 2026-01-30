//! Physical memory layout and address translation helpers.
use core::sync::atomic::{AtomicUsize, Ordering};

use kplat::memory::{HwMemory, MemRange, PhysAddr, VirtAddr, pa, va};
use rs_fdtree::LinuxFdt;
use spin::Once;

use crate::config::{
    devices::MMIO_RANGES,
    plat::{PHYS_MEMORY_BASE, PHYS_MEMORY_SIZE, PHYS_VIRT_OFFSET},
};
const FDT_MEM_SIZE: usize = 0x20_0000;
static FDT_MEM_BASE: AtomicUsize = AtomicUsize::new(0);
static FDT_MEM: Once<[MemRange; 2]> = Once::new();
static DICE_MEM_BASE: AtomicUsize = AtomicUsize::new(0);
static DICE_MEM_SIZE: AtomicUsize = AtomicUsize::new(0);
/// Capture FDT/DICE memory ranges before the allocator is initialized.
pub(crate) fn early_init(fdt_paddr: usize) {
    FDT_MEM_BASE.store(fdt_paddr, Ordering::SeqCst);
    let fdt = unsafe { LinuxFdt::from_ptr(fdt_paddr as *const u8).expect("Failed to parse FDT") };
    fdt.dice().map(|dice_node| {
        let dice = dice_node;
        for reg in dice.regions().expect("DICE regions") {
            DICE_MEM_BASE.store(reg.starting_address as usize, Ordering::SeqCst);
            DICE_MEM_SIZE.store(reg.size as usize, Ordering::SeqCst);
            break;
        }
    });
}
/// Platform-specific memory description for the kernel.
struct HwMemoryImpl;
#[impl_dev_interface]
impl HwMemory for HwMemoryImpl {
    fn ram_regions() -> &'static [MemRange] {
        &[(PHYS_MEMORY_BASE, PHYS_MEMORY_SIZE)]
    }

    /// Returns all reserved physical memory ranges on the platform.
    ///
    /// Reserved memory can be contained in [`ram_regions`], they are not
    /// allocatable but should be mapped to kernel's address space.
    fn rsvd_regions() -> &'static [MemRange] {
        FDT_MEM
            .call_once(|| {
                [
                    (FDT_MEM_BASE.load(Ordering::Relaxed), FDT_MEM_SIZE),
                    (
                        DICE_MEM_BASE.load(Ordering::Relaxed),
                        DICE_MEM_SIZE.load(Ordering::Relaxed),
                    ),
                ]
            })
            .as_ref()
    }

    /// Returns all device memory (MMIO) ranges on the platform.
    fn mmio_regions() -> &'static [MemRange] {
        &MMIO_RANGES
    }

    fn p2v(paddr: PhysAddr) -> VirtAddr {
        va!(paddr.as_usize() + PHYS_VIRT_OFFSET)
    }

    fn v2p(vaddr: VirtAddr) -> PhysAddr {
        pa!(vaddr.as_usize() - PHYS_VIRT_OFFSET)
    }

    fn kernel_layout() -> (VirtAddr, usize) {
        (
            va!(crate::config::plat::KERNEL_ASPACE_BASE),
            crate::config::plat::KERNEL_ASPACE_SIZE,
        )
    }
}

//! Raspberry Pi memory layout implementation for `kplat::memory::HwMemory`.
use kplat::memory::{HwMemory, PhysAddr, RawRange, VirtAddr, pa, va};
use crate::config::devices::MMIO_RANGES;
use crate::config::plat::{PHYS_MEMORY_BASE, PHYS_MEMORY_SIZE, PHYS_VIRT_OFFSET};
struct HwMemoryImpl;
#[impl_dev_interface]
impl HwMemory for HwMemoryImpl {
    fn ram_regions() -> &'static [RawRange] {
        &[(PHYS_MEMORY_BASE, PHYS_MEMORY_SIZE)]
    }
    /// Returns all reserved physical memory ranges on the platform.
    ///
    /// Reserved memory can be contained in [`ram_regions`], they are not
    /// allocatable but should be mapped to kernel's address space.
    fn reserved_ram_regions() -> &'static [RawRange] {
        &[(0, 0x1000)] // spintable
    }
    /// Returns all device memory (MMIO) ranges on the platform.
    fn mmio_regions() -> &'static [RawRange] {
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

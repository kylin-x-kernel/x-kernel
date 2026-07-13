// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform MMIO preparation hooks.

#[cfg(feature = "kvm-mmio-guard")]
use klazy::Once;
#[cfg(feature = "kvm-mmio-guard")]
use kplat::mmio::PlatformMmioIf;

#[cfg(feature = "kvm-mmio-guard")]
static GUARD_GRANULE_BYTES: Once<usize> = Once::new();

pub(crate) fn prepare_boot_memory() {
    #[cfg(feature = "kvm-mmio-guard")]
    initialize_guard_granule();
}

#[cfg(feature = "kvm-mmio-guard")]
fn initialize_guard_granule() {
    let (guard_granule_bytes, guard_has_range) = aarch64_peripherals::kvm::mmio_guard_info();
    assert!(guard_has_range);
    GUARD_GRANULE_BYTES.call_once(|| guard_granule_bytes);
}

#[cfg(feature = "kvm-mmio-guard")]
fn invoke_mmio_guard_map(phys_addr: usize, granule_count: usize) -> usize {
    aarch64_peripherals::kvm::mmio_guard_map(phys_addr, granule_count)
}

#[cfg(feature = "kvm-mmio-guard")]
fn map_guarded_granules(phys_addr: usize, size_bytes: usize) {
    let guard_granule_bytes = *GUARD_GRANULE_BYTES
        .get()
        .expect("KVM MMIO guard granule must be initialized before MMIO mapping");
    let total_granules = size_bytes / guard_granule_bytes;
    let mut mapped_granules = 0usize;
    let mut current_phys_addr = phys_addr;

    while mapped_granules < total_granules {
        let remaining_granules = total_granules - mapped_granules;
        let newly_mapped_granules = invoke_mmio_guard_map(current_phys_addr, remaining_granules);
        assert!(newly_mapped_granules > 0 && newly_mapped_granules <= remaining_granules);
        mapped_granules += newly_mapped_granules;
        current_phys_addr += newly_mapped_granules * guard_granule_bytes;
    }
}

#[cfg(feature = "kvm-mmio-guard")]
#[impl_dev_interface]
impl PlatformMmioIf {
    fn prepare(paddr: usize, size: usize) -> kerrno::KResult {
        map_guarded_granules(paddr, size);
        Ok(())
    }
}

#[cfg(not(feature = "kvm-mmio-guard"))]
kplat::default_mmio_if_impl!();

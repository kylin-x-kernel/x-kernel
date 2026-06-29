// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform DMA preparation hooks.

#[cfg(feature = "kvm-guest-mem-share")]
use khal::mem::PAGE_SIZE_4K;
#[cfg(feature = "kvm-guest-mem-share")]
use kplat::dma::PlatformDmaIf;

struct DmaPlatformImpl;

#[cfg(feature = "kvm-guest-mem-share")]
#[impl_dev_interface]
impl PlatformDmaIf for DmaPlatformImpl {
    fn prepare(paddr: usize, size: usize) -> kerrno::KResult {
        dma_share_pages(paddr, size);
        Ok(())
    }

    fn release(paddr: usize, size: usize) -> kerrno::KResult {
        dma_unshare_pages(paddr, size);
        Ok(())
    }
}

#[cfg(not(feature = "kvm-guest-mem-share"))]
kplat::default_dma_if_impl!(DmaPlatformImpl);

#[cfg(feature = "kvm-guest-mem-share")]
fn dma_unshare_pages(paddr: usize, size: usize) {
    let pages = size / PAGE_SIZE_4K;
    for page_index in 0..pages {
        let page_paddr = paddr + PAGE_SIZE_4K * page_index;
        if !aarch64_peripherals::kvm::guest_mem_unshare_page(page_paddr) {
            log::warn!("[virtio hal impl] cannot unshare 0x{page_paddr:x}");
        }
    }
}

#[cfg(feature = "kvm-guest-mem-share")]
fn dma_share_pages(paddr: usize, size: usize) {
    let pages = size / PAGE_SIZE_4K;
    for page_index in 0..pages {
        let page_paddr = paddr + PAGE_SIZE_4K * page_index;
        if !aarch64_peripherals::kvm::guest_mem_share_page(page_paddr) {
            log::warn!("[virtio hal impl] cannot share 0x{page_paddr:x}");
        }
    }
}

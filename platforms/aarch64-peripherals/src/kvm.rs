// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! KVM vendor hypercall helpers shared by AArch64 guest platforms.
#![cfg(any(feature = "kvm-guest-mem-share", feature = "kvm-mmio-guard"))]

use crate::smccc;

#[cfg(feature = "kvm-guest-mem-share")]
const ARM_SMCCC_VENDOR_HYP_KVM_MEM_SHARE_FUNC_ID: u32 =
    ((1) << 31) | ((1) << 30) | (((6) & 0x3F) << 24) | ((3) & 0xFFFF);
#[cfg(feature = "kvm-guest-mem-share")]
const ARM_SMCCC_VENDOR_HYP_KVM_MEM_UNSHARE_FUNC_ID: u32 =
    ((1) << 31) | ((1) << 30) | (((6) & 0x3F) << 24) | ((4) & 0xFFFF);
#[cfg(feature = "kvm-mmio-guard")]
const ARM_SMCCC_VENDOR_HYP_KVM_MMIO_GUARD_INFO_FUNC_ID: u32 =
    ((1) << 31) | ((1) << 30) | (((6) & 0x3F) << 24) | ((5) & 0xFFFF);
#[cfg(feature = "kvm-mmio-guard")]
const ARM_SMCCC_VENDOR_HYP_KVM_MMIO_GUARD_MAP_FUNC_ID: u32 =
    ((1) << 31) | ((1) << 30) | (((6) & 0x3F) << 24) | ((10) & 0xFFFF);

/// Return the KVM MMIO guard granule size and whether the hypervisor exposes
/// guarded MMIO ranges for the current guest.
#[cfg(feature = "kvm-mmio-guard")]
pub fn mmio_guard_info() -> (usize, bool) {
    let result = smccc::hvc_call(ARM_SMCCC_VENDOR_HYP_KVM_MMIO_GUARD_INFO_FUNC_ID, 0, 0, 0);
    (result.x0, result.x1 == 0x1)
}

/// Ask KVM to map up to `granule_count` guarded MMIO granules starting at
/// `phys_addr`. Returns the number of granules mapped in this call.
#[cfg(feature = "kvm-mmio-guard")]
pub fn mmio_guard_map(phys_addr: usize, granule_count: usize) -> usize {
    let result = smccc::hvc_call(
        ARM_SMCCC_VENDOR_HYP_KVM_MMIO_GUARD_MAP_FUNC_ID,
        phys_addr,
        granule_count,
        0,
    );
    assert_eq!(result.x0, 0);
    result.x1
}

/// Ask KVM to share one guest page with the host.
#[cfg(feature = "kvm-guest-mem-share")]
pub fn guest_mem_share_page(page_paddr: usize) -> bool {
    let result = smccc::hvc_call(ARM_SMCCC_VENDOR_HYP_KVM_MEM_SHARE_FUNC_ID, page_paddr, 1, 0);
    result.x0 == 0
}

/// Ask KVM to unshare one guest page from the host.
#[cfg(feature = "kvm-guest-mem-share")]
pub fn guest_mem_unshare_page(page_paddr: usize) -> bool {
    let result = smccc::hvc_call(
        ARM_SMCCC_VENDOR_HYP_KVM_MEM_UNSHARE_FUNC_ID,
        page_paddr,
        1,
        0,
    );
    result.x0 == 0
}

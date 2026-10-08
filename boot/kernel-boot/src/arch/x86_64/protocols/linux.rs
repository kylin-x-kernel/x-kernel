// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux boot protocol contract for future `QEMU --kernel` direct boot on x86_64.
//!
//! The actual direct entrypoint is not wired yet. This module defines the
//! handoff contract expected by the unified x86 boot layer:
//! - `BootInfo.protocol = BootProtocol::LinuxBoot`
//! - `BootInfo.protocol_info_addr = struct boot_params *`
//! - `BootInfo.rsdp_addr` may be populated directly by the loader or derived
//!   later from the Linux zeropage.

use boot_info::LinuxBootParams;

/// Returns a view over the zeropage at `protocol_info_addr`, or `None` when it
/// is zero.
///
/// # Safety
///
/// `protocol_info_addr` must satisfy the [`LinuxBootParams::new`] contract.
/// In particular, call this only while the zeropage at that address is still
/// mapped in the current address space, normally by the boot stub's temporary
/// identity mapping of low memory, and is not being written. Once the kernel
/// page table replaces that mapping, a raw physical zeropage address no longer
/// meets the contract.
#[inline]
pub unsafe fn boot_params(protocol_info_addr: usize) -> Option<LinuxBootParams> {
    // SAFETY: this function's caller upholds the `LinuxBootParams::new`
    // contract for `protocol_info_addr`, as required by its `# Safety` section.
    unsafe { LinuxBootParams::new(protocol_info_addr) }
}

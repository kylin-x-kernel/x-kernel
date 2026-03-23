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

#[inline]
pub fn boot_params(protocol_info_addr: usize) -> Option<LinuxBootParams> {
    LinuxBootParams::new(protocol_info_addr)
}

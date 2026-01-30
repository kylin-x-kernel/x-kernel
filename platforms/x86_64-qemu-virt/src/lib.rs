// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform support for x86_64-qemu-virt.

#![cfg(target_arch = "x86_64")]
#![no_std]
#[macro_use]
extern crate log;
#[macro_use]
extern crate kplat;
mod apic;
mod boot;
mod console;
mod init;
mod mem;
#[cfg(feature = "smp")]
mod mp;
mod power;
mod time;
pub mod config {
    platconfig_macros::include_configs!(path_env = "PLAT_CONFIG_PATH", fallback = "axconfig.toml");
}
fn current_cpu_id() -> usize {
    match raw_cpuid::CpuId::new().get_feature_info() {
        Some(finfo) => finfo.initial_local_apic_id() as usize,
        None => 0,
    }
}
unsafe extern "C" fn rust_entry(magic: usize, mbi: usize) {
    if magic == self::boot::MULTIBOOT_BOOTLOADER_MAGIC {
        kplat::entry(current_cpu_id(), mbi);
    }
}
unsafe extern "C" fn rust_entry_secondary(_magic: usize) {
    #[cfg(feature = "smp")]
    if _magic == self::boot::MULTIBOOT_BOOTLOADER_MAGIC {
        kplat::entry_secondary(current_cpu_id());
    }
}

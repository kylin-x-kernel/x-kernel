// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use boot_info::BootInfo;
use kernel_boot::bootln;

use super::{CMDLINE_BUF_SIZE, state};

pub(super) fn init(boot_info: &BootInfo) {
    if let Some(ptr) = boot_info.dtb_ptr() {
        bootln!("firmware: DTB at {:#x}", boot_info.dtb_addr);
        // SAFETY: `ptr` comes from validated boot info and points to the
        // immutable device-tree blob selected for early firmware parsing.
        if let Err(err) = unsafe { of::init_device_tree_ptr(ptr) } {
            bootln!("firmware: failed to initialize DTB: {err:?}");
        }
        if let Some(stdout_path) = of::chosen_stdout_path() {
            bootln!("firmware: chosen stdout-path={}", stdout_path);
        } else {
            bootln!("firmware: chosen stdout-path=<none>");
        }
        if let Some(bootargs) = of::chosen_bootargs() {
            bootln!("firmware: chosen bootargs={}", bootargs);
        } else {
            bootln!("firmware: chosen bootargs=<none>");
        }
    }

    if boot_info.rsdp_addr != 0 {
        bootln!("firmware: ACPI RSDP at {:#x}", boot_info.rsdp_addr);
        let _ = acpi::init(boot_info.rsdp_addr);
    }

    bootln!(
        "firmware: hwdesc source={}",
        boot_info.hardware_description_root().name()
    );
    bootln!(
        "firmware: memory source={}",
        boot_info.memory_description_root().name()
    );
    kplat::boot::firmware_init(boot_info);

    let mut cmdline_buf = [0; CMDLINE_BUF_SIZE];
    let cmdline_len = if let Some(cmdline) = boot_info.cmdline() {
        bootln!("firmware: boot cmdline={}", cmdline);
        let bytes = cmdline.as_bytes();
        let len = bytes.len().min(CMDLINE_BUF_SIZE);
        cmdline_buf[..len].copy_from_slice(&bytes[..len]);
        len
    } else if let Some(cmdline) = of::chosen_bootargs() {
        bootln!("firmware: using chosen bootargs as cmdline={}", cmdline);
        let bytes = cmdline.as_bytes();
        let len = bytes.len().min(CMDLINE_BUF_SIZE);
        cmdline_buf[..len].copy_from_slice(&bytes[..len]);
        len
    } else {
        bootln!("firmware: no cmdline");
        0
    };
    state::init_cmdline(cmdline_buf, cmdline_len);
}

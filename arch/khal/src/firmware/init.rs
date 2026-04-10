// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use boot_info::BootInfo;
use kernel_boot::bootln;

use super::{CMDLINE_BUF_SIZE, DTB_CAPTURE_SIZE, state};
use crate::mem;

pub(super) fn init(boot_info: &BootInfo) {
    let dtb_vaddr = if boot_info.dtb_addr != 0 {
        Some(mem::p2v(boot_info.dtb_addr.into()).as_usize() as *const u8)
    } else {
        None
    };
    if let Some(ptr) = dtb_vaddr {
        bootln!("firmware: DTB at {:#x}", boot_info.dtb_addr);
        state::init_dtb_capture(boot_info.dtb_addr, DTB_CAPTURE_SIZE);
        bootln!(
            "firmware: DTB capture region={:#x}..{:#x}",
            boot_info.dtb_addr,
            boot_info.dtb_addr + DTB_CAPTURE_SIZE
        );
        let _ = unsafe { of::init_device_tree_ptr(ptr) };
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
    } else if boot_info.rsdp_addr != 0 {
        bootln!("firmware: ACPI RSDP at {:#x}", boot_info.rsdp_addr);
        let _ = acpi::init(boot_info.rsdp_addr);
    }
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

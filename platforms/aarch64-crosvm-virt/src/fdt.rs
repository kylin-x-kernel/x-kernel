// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device tree parsing helpers for the platform.
use kplat::memory::{VirtAddr, p2v, pa};
use ktypes::Once;
use log::*;
use rs_fdtree::{InterruptController, LinuxFdt};
pub static FDT: Once<LinuxFdt> = Once::new();
/// Parse and cache the FDT pointed to by the bootloader.
pub(crate) fn init_fdt(fdt_vaddr: VirtAddr) {
    info!("FDT addr is: {:x}", fdt_vaddr.as_usize());
    let fdt = unsafe {
        LinuxFdt::from_ptr(fdt_vaddr.as_usize() as *const u8).expect("Failed to parse FDT")
    };
    FDT.call_once(|| fdt);
    dice_reg();
}
#[allow(dead_code)]
/// Lookup the interrupt-controller node from the cached FDT.
pub(crate) fn interrupt_controller() -> Option<InterruptController<'static, 'static>> {
    let fdt = FDT.get().expect("FDT is not initialized");
    match fdt.interrupt_controller() {
        Some(ic_node) => Some(ic_node),
        None => {
            warn!("No interrupt-controller node found in FDT");
            None
        }
    }
}
/// Return the first DICE MMIO region mapped into kernel space.
pub fn dice_reg() -> Option<(VirtAddr, usize)> {
    let dice = FDT.get().unwrap().dice();
    if let Some(dice_node) = dice {
        info!("Found DICE node in FDT");
        if let Some(reg) = dice_node.regions().expect("DICE regions").next() {
            info!(
                "DICE region: addr=0x{:x}, size=0x{:x}",
                reg.starting_address as usize, reg.size
            );

            let va = p2v(pa!(reg.starting_address as usize));
            return Some((va, reg.size));
        }
    }
    None
}

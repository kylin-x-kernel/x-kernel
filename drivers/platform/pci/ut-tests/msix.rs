// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

extern crate alloc;

use alloc::boxed::Box;

use unittest::{assert_eq, def_test};

use super::*;
use crate::Cam;

// Both config interfaces use the same private RAM window, sequentially, just
// as the production root and copied config accessor address one PCI function.
struct FakeConfig(PciConfigAccess);

impl ConfigurationAccess for FakeConfig {
    fn read_word(&self, bdf: DeviceFunction, offset: u8) -> u32 {
        self.0.read_word(bdf, u16::from(offset))
    }

    fn write_word(&mut self, bdf: DeviceFunction, offset: u8, value: u32) {
        self.0.write_word(bdf, u16::from(offset), value);
    }

    unsafe fn unsafe_clone(&self) -> Self {
        Self(self.0)
    }
}

#[def_test]
#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
)]
#[allow(
    unknown_lints,
    std_box_in_kernel_tests,
    reason = "CG-049-no-std-box-in-kernel-tests: portable PCI config accessors require \
              program-lifetime aligned backing shared by copied accessors; a kernel allocator \
              dependency is forbidden"
)]
fn shutdown_never_restores_discovery_time_msix_enable() {
    let bdf = DeviceFunction {
        bus: 0,
        device: 0,
        function: 0,
    };
    for use_root in [false, true] {
        for enabled in [false, true] {
            for function_masked in [false, true] {
                // PciConfigAccess requires program-lifetime backing. This small
                // leaked window covers all accesses to BDF 0, offsets 0..255.
                let window = Box::leak(Box::new([0u32; 64]));
                // SAFETY: aligned RAM simulates config MMIO and is retained
                // forever. Only these sequential volatile accessors use it;
                // no hardware, shared fixture or out-of-window BDF is accessed.
                let mut config =
                    unsafe { PciConfigAccess::new(window.as_mut_ptr().cast(), Cam::MmioCam) };
                let cap = MsixCapability {
                    cap_offset: 0x40,
                    table_size: 2,
                    enabled,
                    function_masked,
                    table_bar: 0,
                    table_offset: 0,
                    pba_bar: 0,
                    pba_offset: 0,
                };
                // Model an active, unmasked function even if discovery saw a
                // different state. Preserve table-size and capability fields.
                let original = 0x0123_5011 | MSIX_ENABLE;
                config.write_word(bdf, cap.cap_offset, original);
                let command = Command::MEMORY_SPACE | Command::BUS_MASTER;
                config.write_word(
                    bdf,
                    STATUS_COMMAND_OFFSET,
                    u32::from((command | Command::INTERRUPT_DISABLE).bits()),
                );
                let mut root = PciRoot::new(FakeConfig(config));
                for _ in 0..2 {
                    if use_root {
                        disable_msix(&mut root, &mut config, bdf, &cap);
                    } else {
                        disable_msix_with_config(&mut config, bdf, &cap);
                    }
                    assert_eq!(
                        config.read_word(bdf, cap.cap_offset),
                        (original | MSIX_FUNCTION_MASK) & !MSIX_ENABLE
                    );
                    assert_eq!(
                        config.read_word(bdf, STATUS_COMMAND_OFFSET) as u16,
                        command.bits()
                    );
                }
            }
        }
    }
}

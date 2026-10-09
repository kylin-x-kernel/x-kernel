// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared boot console configuration helpers.

#[cfg(any(
    target_arch = "aarch64",
    target_arch = "riscv64",
    target_arch = "loongarch64"
))]
#[inline]
pub(crate) fn is_mmio_configured() -> bool {
    kbuild_config::BOOT_CONSOLE_TYPE == "mmio" && kbuild_config::BOOT_CONSOLE_ADDR != 0
}

#[cfg(any(
    target_arch = "aarch64",
    target_arch = "riscv64",
    target_arch = "loongarch64"
))]
#[inline]
pub(crate) fn mmio_addr() -> Option<usize> {
    if is_mmio_configured() {
        Some(kbuild_config::BOOT_CONSOLE_ADDR)
    } else {
        None
    }
}

#[cfg(target_arch = "x86_64")]
#[inline]
fn is_ioport_configured() -> bool {
    kbuild_config::BOOT_CONSOLE_TYPE == "ioport" && kbuild_config::BOOT_CONSOLE_ADDR != 0
}

#[cfg(target_arch = "x86_64")]
#[inline]
pub(crate) fn ioport_addr() -> Option<u16> {
    if !is_ioport_configured() {
        return None;
    }
    Some({
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: the configured early UART \
                      address is absent or invalid; boot-console access has no error return or \
                      recovery"
        )]
        u16::try_from(kbuild_config::BOOT_CONSOLE_ADDR)
            .expect("boot console ioport must fit in u16")
    })
}

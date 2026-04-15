// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::fmt::{self, Write};

#[cfg(target_arch = "aarch64")]
use crate::arch::aarch64::serial as imp;
#[cfg(target_arch = "loongarch64")]
use crate::arch::loongarch64::serial as imp;
#[cfg(target_arch = "riscv64")]
use crate::arch::riscv64::serial as imp;
#[cfg(target_arch = "x86_64")]
use crate::arch::x86_64::serial as imp;

#[cfg(not(any(
    target_arch = "aarch64",
    target_arch = "loongarch64",
    target_arch = "riscv64",
    target_arch = "x86_64"
)))]
mod imp {
    pub fn is_enabled() -> bool {
        false
    }

    pub fn write_str(_data: &str) {}

    pub fn write_hex(_value: usize) {}
}

struct BootConsoleWriter;

impl Write for BootConsoleWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        imp::write_str(s);
        Ok(())
    }
}

#[inline]
pub fn is_enabled() -> bool {
    imp::is_enabled()
}

#[inline]
pub fn write_str(data: &str) {
    imp::write_str(data);
}

#[inline]
pub fn write_hex(value: usize) {
    imp::write_hex(value);
}

#[inline]
pub fn log(args: fmt::Arguments<'_>) {
    if !is_enabled() {
        return;
    }
    let mut writer = BootConsoleWriter;
    let _ = writer.write_fmt(args);
}

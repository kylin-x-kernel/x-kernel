// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Minimal early-boot UART printing for RISC-V diagnostics.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kaddr_layout::PAGE_OFFSET;

use crate::bootconsole_config;

const BOOT_PREFIX: &[u8] = b"[boot] ";
const UART_THR_OFFSET: usize = 0;
const UART_LSR_OFFSET: usize = 5;
const UART_LSR_THR_EMPTY: u8 = 1 << 5;

static BOOT_CONSOLE_BASE: AtomicUsize = AtomicUsize::new(kbuild_config::BOOT_CONSOLE_ADDR);
static BOOT_CONSOLE_START_OF_LINE: AtomicBool = AtomicBool::new(true);

#[inline]
pub(crate) fn is_enabled() -> bool {
    BOOT_CONSOLE_BASE.load(Ordering::Relaxed) != 0
}

#[inline]
fn active_base() -> Option<usize> {
    let base = BOOT_CONSOLE_BASE.load(Ordering::Relaxed);
    if base == 0 { None } else { Some(base) }
}

#[unsafe(link_section = ".text.boot")]
pub(crate) fn activate_linear_map() {
    let Some(paddr) = bootconsole_config::mmio_addr() else {
        BOOT_CONSOLE_BASE.store(0, Ordering::Relaxed);
        return;
    };
    BOOT_CONSOLE_BASE.store(PAGE_OFFSET + paddr, Ordering::Relaxed);
}

#[inline]
fn write_raw_byte(byte: u8) {
    let Some(base) = active_base() else {
        return;
    };
    let lsr = (base + UART_LSR_OFFSET) as *const u8;
    let thr = (base + UART_THR_OFFSET) as *mut u8;
    unsafe {
        while lsr.read_volatile() & UART_LSR_THR_EMPTY == 0 {}
        thr.write_volatile(byte);
    }
}

#[inline]
fn write_prefixed_byte(byte: u8) {
    if BOOT_CONSOLE_START_OF_LINE.swap(false, Ordering::Relaxed) {
        for &prefix in BOOT_PREFIX {
            write_raw_byte(prefix);
        }
    }

    write_raw_byte(byte);
    if byte == b'\n' {
        BOOT_CONSOLE_START_OF_LINE.store(true, Ordering::Relaxed);
    }
}

pub(crate) fn write_str(data: &str) {
    for byte in data.bytes() {
        write_prefixed_byte(byte);
    }
}

pub(crate) fn write_hex(num: usize) {
    let mut digits = [0u8; 16];
    let mut num = num;
    let mut cnt = 0;

    write_str("0x");
    if num == 0 {
        write_prefixed_byte(b'0');
        return;
    }

    while num != 0 {
        digits[cnt] = match (num & 0xf) as u8 {
            n if n < 10 => n + b'0',
            n => n - 10 + b'a',
        };
        cnt += 1;
        num >>= 4;
    }

    for idx in (0..cnt).rev() {
        write_prefixed_byte(digits[idx]);
    }
}

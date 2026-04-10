// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Minimal early-boot serial printing for x86_64 diagnostics.

use core::sync::atomic::{AtomicBool, Ordering};

use crate::bootconsole_config;

const BOOT_PREFIX: &[u8] = b"[boot] ";
const UART_DATA_OFFSET: u16 = 0;
const UART_IER_OFFSET: u16 = 1;
const UART_FCR_OFFSET: u16 = 2;
const UART_LCR_OFFSET: u16 = 3;
const UART_MCR_OFFSET: u16 = 4;
const UART_LSR_OFFSET: u16 = 5;

const UART_IER_DISABLE_ALL: u8 = 0x00;
const UART_LCR_DLAB: u8 = 0x80;
const UART_LCR_8N1: u8 = 0x03;
const UART_DLL_38400_BAUD: u8 = 0x03;
const UART_DLM_38400_BAUD: u8 = 0x00;
const UART_FCR_ENABLE_AND_CLEAR: u8 = 0xC7;
const UART_MCR_IRQ_RTS_DTR: u8 = 0x0B;
const UART_LSR_THR_EMPTY: u8 = 1 << 5;

static BOOT_CONSOLE_INITIALIZED: AtomicBool = AtomicBool::new(false);
static BOOT_CONSOLE_START_OF_LINE: AtomicBool = AtomicBool::new(true);

#[inline]
pub(crate) fn is_enabled() -> bool {
    bootconsole_config::ioport_addr().is_some()
}

#[inline]
fn init_once() {
    if BOOT_CONSOLE_INITIALIZED.load(Ordering::Acquire) {
        return;
    }

    let port = bootconsole_config::ioport_addr().expect("missing x86 boot console ioport");
    unsafe {
        outb(port + UART_IER_OFFSET, UART_IER_DISABLE_ALL);
        outb(port + UART_LCR_OFFSET, UART_LCR_DLAB);
        outb(port + UART_DATA_OFFSET, UART_DLL_38400_BAUD);
        outb(port + UART_IER_OFFSET, UART_DLM_38400_BAUD);
        outb(port + UART_LCR_OFFSET, UART_LCR_8N1);
        outb(port + UART_FCR_OFFSET, UART_FCR_ENABLE_AND_CLEAR);
        outb(port + UART_MCR_OFFSET, UART_MCR_IRQ_RTS_DTR);
    }

    BOOT_CONSOLE_INITIALIZED.store(true, Ordering::Release);
}

#[inline]
fn write_raw_byte(byte: u8) {
    init_once();
    let port = bootconsole_config::ioport_addr().expect("missing x86 boot console ioport");
    unsafe {
        while inb(port + UART_LSR_OFFSET) & UART_LSR_THR_EMPTY == 0 {}
        outb(port + UART_DATA_OFFSET, byte);
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

#[inline]
unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    unsafe {
        core::arch::asm!(
            "in al, dx",
            in("dx") port,
            out("al") value,
            options(nomem, nostack, preserves_flags)
        );
    }
    value
}

#[inline]
unsafe fn outb(port: u16, value: u8) {
    unsafe {
        core::arch::asm!(
            "out dx, al",
            in("dx") port,
            in("al") value,
            options(nomem, nostack, preserves_flags)
        );
    }
}

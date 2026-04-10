// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Minimal early-boot UART printing for diagnostics.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kaddr_layout::{BOOT_IO_SLOT_SIZE, BOOT_IO_VSIZE, BOOT_UART_SLOT, BOOT_UART_SLOT_VADDR};
use memaddr::{MemoryAddr, PAGE_SIZE_4K};

use crate::bootconsole_config;

const BOOT_PREFIX: &[u8] = b"[boot] ";
pub(crate) const BOOT_UART_BOOT_VADDR: usize =
    BOOT_UART_SLOT_VADDR + (kbuild_config::BOOT_CONSOLE_ADDR & (BOOT_IO_SLOT_SIZE - 1));
static BOOT_CONSOLE_BASE: AtomicUsize = AtomicUsize::new(kbuild_config::BOOT_CONSOLE_ADDR);
static BOOT_CONSOLE_START_OF_LINE: AtomicBool = AtomicBool::new(true);

#[inline]
fn assert_boot_uart_fits_boot_io_window() {
    let uart_paddr = bootconsole_config::mmio_addr().expect("missing boot console mmio address");
    let offset = uart_paddr & (BOOT_IO_SLOT_SIZE - 1);
    let span = (offset + PAGE_SIZE_4K).align_up_4k();
    let slot_start = BOOT_UART_SLOT * BOOT_IO_SLOT_SIZE;
    assert!(
        slot_start < BOOT_IO_VSIZE
            && BOOT_IO_SLOT_SIZE <= BOOT_IO_VSIZE.saturating_sub(slot_start)
            && span <= BOOT_IO_SLOT_SIZE,
        "boot UART slot {BOOT_UART_SLOT} exceeds boot IO window {BOOT_IO_VSIZE:#x}"
    );
}

#[inline]
pub(crate) fn is_enabled() -> bool {
    BOOT_CONSOLE_BASE.load(Ordering::Relaxed) != 0
}

#[inline]
fn active_uart() -> Option<Uart> {
    let base = BOOT_CONSOLE_BASE.load(Ordering::Relaxed);
    if base == 0 {
        None
    } else {
        Some(Uart::new(base))
    }
}

#[inline]
#[unsafe(link_section = ".idmap.text")]
fn is_enabled_idmap() -> bool {
    BOOT_CONSOLE_BASE.load(Ordering::Relaxed) != 0
}

#[inline]
#[unsafe(link_section = ".idmap.text")]
fn active_uart_idmap() -> Option<Uart> {
    let base = BOOT_CONSOLE_BASE.load(Ordering::Relaxed);
    if base == 0 {
        None
    } else {
        Some(Uart::new(base))
    }
}

#[inline]
fn write_raw_bytes(bytes: &[u8]) {
    let Some(uart) = active_uart() else {
        return;
    };
    for &byte in bytes {
        let _ = uart.put(byte);
    }
}

#[inline]
#[unsafe(link_section = ".idmap.text")]
fn write_raw_bytes_idmap(bytes: &[u8]) {
    let Some(uart) = active_uart_idmap() else {
        return;
    };
    for &byte in bytes {
        let _ = uart.put_idmap(byte);
    }
}

fn write_prefixed_byte(byte: u8) {
    if !is_enabled() {
        return;
    }

    if BOOT_CONSOLE_START_OF_LINE.swap(false, Ordering::Relaxed) {
        write_raw_bytes(BOOT_PREFIX);
    }
    write_raw_bytes(core::slice::from_ref(&byte));
    if byte == b'\n' {
        BOOT_CONSOLE_START_OF_LINE.store(true, Ordering::Relaxed);
    }
}

#[unsafe(link_section = ".idmap.text")]
fn write_prefixed_byte_idmap(byte: u8) {
    if !is_enabled_idmap() {
        return;
    }

    if BOOT_CONSOLE_START_OF_LINE.swap(false, Ordering::Relaxed) {
        write_raw_bytes_idmap(BOOT_PREFIX);
    }
    write_raw_bytes_idmap(core::slice::from_ref(&byte));
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

#[unsafe(link_section = ".idmap.text")]
fn write_str_idmap(data: &str) {
    for byte in data.bytes() {
        write_prefixed_byte_idmap(byte);
    }
}

#[unsafe(link_section = ".idmap.text")]
fn write_hex_idmap(num: usize) {
    let mut digits = [0u8; 16];
    let mut num = num;
    let mut cnt = 0;

    write_str_idmap("0x");
    if num == 0 {
        write_prefixed_byte_idmap(b'0');
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
        write_prefixed_byte_idmap(digits[idx]);
    }
}

pub(crate) fn activate_boot_map() -> bool {
    if bootconsole_config::mmio_addr().is_none() || BOOT_UART_BOOT_VADDR == 0 {
        return false;
    }
    assert_boot_uart_fits_boot_io_window();
    BOOT_CONSOLE_BASE.store(BOOT_UART_BOOT_VADDR, Ordering::Relaxed);
    true
}

#[unsafe(no_mangle)]
pub extern "C" fn _boot_print_usize(num: usize) {
    if !is_enabled_idmap() {
        return;
    }
    write_hex_idmap(num);
    write_str_idmap("\r\n");
}

#[unsafe(no_mangle)]
#[unsafe(link_section = ".idmap.text")]
pub fn boot_print_str(data: &str) {
    write_str_idmap(data);
}

#[allow(dead_code)]
pub fn boot_print_usize(num: usize) {
    write_hex_idmap(num);
    write_str_idmap("\r\n");
}

#[derive(Copy, Clone, Debug)]
pub struct Uart {
    base_address: usize,
}

impl Uart {
    #[unsafe(link_section = ".idmap.text")]
    pub const fn new(base_address: usize) -> Self {
        Self { base_address }
    }

    #[unsafe(link_section = ".idmap.text")]
    pub fn put(&self, c: u8) -> Option<u8> {
        let ptr = self.base_address as *mut u8;
        unsafe {
            ptr.write_volatile(c);
        }
        Some(c)
    }

    #[unsafe(link_section = ".idmap.text")]
    pub fn put_idmap(&self, c: u8) -> Option<u8> {
        let ptr = self.base_address as *mut u8;
        unsafe {
            ptr.write_volatile(c);
        }
        Some(c)
    }
}

#[allow(dead_code)]
pub fn print_el1_reg(switch: bool) {
    if !switch {
        return;
    }
    crate::boot_print_reg!("SCTLR_EL1");
    crate::boot_print_reg!("SPSR_EL1");
    crate::boot_print_reg!("TCR_EL1");
    crate::boot_print_reg!("VBAR_EL1");
    crate::boot_print_reg!("MAIR_EL1");
    crate::boot_print_reg!("MPIDR_EL1");
    crate::boot_print_reg!("TTBR0_EL1");
    crate::boot_print_reg!("TTBR1_EL1");
    crate::boot_print_reg!("ID_AA64AFR0_EL1");
    crate::boot_print_reg!("ID_AA64AFR1_EL1");
    crate::boot_print_reg!("ID_AA64DFR0_EL1");
    crate::boot_print_reg!("ID_AA64DFR1_EL1");
    crate::boot_print_reg!("ID_AA64ISAR0_EL1");
    crate::boot_print_reg!("ID_AA64ISAR1_EL1");
    crate::boot_print_reg!("ID_AA64ISAR2_EL1");
    crate::boot_print_reg!("ID_AA64MMFR0_EL1");
    crate::boot_print_reg!("ID_AA64MMFR1_EL1");
    crate::boot_print_reg!("ID_AA64MMFR2_EL1");
    crate::boot_print_reg!("ID_AA64PFR0_EL1");
    crate::boot_print_reg!("ID_AA64PFR1_EL1");
    crate::boot_print_reg!("ICC_AP0R0_EL1");
    crate::boot_print_reg!("ICC_AP1R0_EL1");
    crate::boot_print_reg!("ICC_BPR0_EL1");
    crate::boot_print_reg!("ICC_BPR1_EL1");
    crate::boot_print_reg!("ICC_CTLR_EL1");
    crate::boot_print_reg!("ICC_HPPIR0_EL1");
    crate::boot_print_reg!("ICC_HPPIR1_EL1");
    crate::boot_print_reg!("ICC_IAR0_EL1");
    crate::boot_print_reg!("ICC_IAR1_EL1");
    crate::boot_print_reg!("ICC_IGRPEN0_EL1");
    crate::boot_print_reg!("ICC_IGRPEN1_EL1");
    crate::boot_print_reg!("ICC_PMR_EL1");
    crate::boot_print_reg!("ICC_RPR_EL1");
    crate::boot_print_reg!("ICC_SRE_EL1");
}

#[macro_export]
macro_rules! boot_print_reg {
    ($reg_name:tt) => {
        boot_print_str($reg_name);
        boot_print_str(": ");
        let reg;
        unsafe { core::arch::asm!(concat!("mrs {}, ", $reg_name), out(reg) reg) };
        boot_print_usize(reg);
    };
}

#[allow(unused)]
#[unsafe(link_section = ".idmap.text")]
pub fn boot_serial_send(data: u8) {
    write_prefixed_byte(data);
}

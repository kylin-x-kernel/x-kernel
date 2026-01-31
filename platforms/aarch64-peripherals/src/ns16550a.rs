// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! NS16550A UART helper functions and console adapter macro.
use kplat::memory::VirtAddr;
use kspin::SpinNoIrq;
use lazyinit::LazyInit;
use uart_16550::{MmioSerialPort, WouldBlockError};
static UART: LazyInit<SpinNoIrq<MmioSerialPort>> = LazyInit::new();
/// Write one byte to the UART, translating LF to CRLF.
fn do_putchar(uart: &mut MmioSerialPort, c: u8) {
    match c {
        b'\n' => {
            uart.send(b'\r');
            uart.send(b'\n');
        }
        c => uart.send(c),
    }
}
/// Write bytes to the UART using a temporary MMIO mapping.
pub fn write_data_force(uart_base: VirtAddr, bytes: &[u8]) {
    let base_addr = uart_base.as_usize();
    let mut uart = unsafe { MmioSerialPort::new(base_addr) };
    uart.init();
    for c in bytes {
        do_putchar(&mut uart, *c);
    }
}
/// Write a single byte to the shared UART instance.
pub fn putchar(c: u8) {
    do_putchar(&mut UART.lock(), c);
}
/// Try to read a single byte from the UART.
pub fn getchar<E>() -> Result<u8, WouldBlockError> {
    UART.lock().try_receive()
}
/// Write bytes to the shared UART instance.
pub fn write_data(bytes: &[u8]) {
    let mut uart = UART.lock();
    for c in bytes {
        do_putchar(&mut uart, *c);
    }
}
/// Read available bytes into the buffer and return the count.
pub fn read_data(bytes: &mut [u8]) -> usize {
    let mut read_len = 0;
    while read_len < bytes.len() {
        if let Ok(c) = getchar::<WouldBlockError>() {
            bytes[read_len] = c;
            read_len += 1;
        } else {
            break;
        }
    }
    read_len
}
/// Initialize the shared UART instance from the given base address.
pub fn early_init(uart_base: VirtAddr) {
    UART.init_once(SpinNoIrq::new({
        let base_addr = uart_base.as_usize();
        let mut uart = unsafe { MmioSerialPort::new(base_addr) };
        uart.init();
        uart
    }));
}
/// Implement `kplat::io::ConsoleIf` using the NS16550A backend.
#[allow(clippy::crate_in_macro_def)]
#[macro_export]
macro_rules! ns16550_console_if_impl {
    ($name:ident) => {
        struct $name;
        #[kplat::impl_dev_interface]
        impl kplat::io::ConsoleIf for $name {
            fn write_data(bytes: &[u8]) {
                $crate::ns16550a::write_data(bytes);
            }

            fn write_data_atomic(bytes: &[u8]) {
                let mut uart_base =
                    kplat::memory::p2v(kplat::memory::pa!(crate::config::devices::UART_PADDR));
                $crate::ns16550a::write_data_force(uart_base, bytes);
            }

            fn read_data(bytes: &mut [u8]) -> usize {
                $crate::ns16550a::read_data(bytes)
            }

            fn interrupt_id() -> Option<usize> {
                Some(crate::config::devices::UART_IRQ as _)
            }
        }
    };
}

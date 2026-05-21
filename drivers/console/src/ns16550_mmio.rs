// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use khal::mem::VirtAddr;
use kspin::SpinNoIrq;
use lazyinit::LazyInit;
use uart_16550::MmioSerialPort;

static UART: LazyInit<SpinNoIrq<MmioSerialPort>> = LazyInit::new();
struct DriverConsoleIfImpl;

#[kplat::impl_dev_interface]
impl khal::console::ConsoleIf for DriverConsoleIfImpl {
    fn write_data(bytes: &[u8]) {
        crate::write_data(bytes);
    }

    fn write_data_atomic(bytes: &[u8]) {
        crate::write_data(bytes);
    }

    fn read_data(bytes: &mut [u8]) -> usize {
        crate::read_data(bytes)
    }

    fn interrupt_id() -> Option<usize> {
        crate::interrupt_id()
    }
}

fn do_putchar(uart: &mut MmioSerialPort, c: u8) {
    match c {
        b'\n' => {
            uart.send(b'\r');
            uart.send(b'\n');
        }
        c => uart.send(c),
    }
}

fn with_uart_mut<R>(f: impl FnOnce(&mut MmioSerialPort) -> R) -> R {
    let mut uart = UART.lock();
    f(&mut uart)
}

pub fn init(uart_base: VirtAddr) {
    UART.init_once(SpinNoIrq::new({
        let mut uart = unsafe { MmioSerialPort::new(uart_base.as_usize()) };
        uart.init();
        uart
    }));
}

pub fn getchar() -> Option<u8> {
    with_uart_mut(|uart| uart.try_receive().ok())
}

pub fn write_data(bytes: &[u8]) {
    with_uart_mut(|uart| {
        for &c in bytes {
            do_putchar(uart, c);
        }
    });
}

pub fn read_data(bytes: &mut [u8]) -> usize {
    let mut read_len = 0;
    while read_len < bytes.len() {
        if let Ok(c) = with_uart_mut(|uart| uart.try_receive()) {
            bytes[read_len] = c;
            read_len += 1;
        } else {
            break;
        }
    }
    read_len
}

pub fn ack_interrupt() {}

pub(super) fn init_backend(config: crate::ConsoleConfig) {
    match config.transport {
        crate::ConsoleTransport::Mmio { paddr, size } => {
            let uart_base = memspace::iomap_device(paddr, size, "console-ns16550")
                .unwrap_or_else(|err| panic!("failed to iomap console: {err:?}"));
            init(uart_base);
        }
        crate::ConsoleTransport::IoPort { .. } => panic!("ns16550-mmio does not support ioport transport"),
    }
}

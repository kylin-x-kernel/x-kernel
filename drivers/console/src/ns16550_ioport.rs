// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kspin::SpinNoIrq;
use lazyinit::LazyInit;
use uart_16550::SerialPort;

static SERIAL: LazyInit<SpinNoIrq<SerialPort>> = LazyInit::new();
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

pub fn init(io_port: u16) {
    SERIAL.init_once({
        // SAFETY: `io_port` is the configured NS16550 base I/O port and is
        // only used here to construct the early console device instance.
        let mut uart = unsafe { SerialPort::new(io_port) };
        uart.init();
        SpinNoIrq::new(uart)
    });
}

pub fn getchar() -> Option<u8> {
    SERIAL.lock().try_receive().ok()
}

pub fn write_data(bytes: &[u8]) {
    let mut uart = SERIAL.lock();
    for &c in bytes {
        uart.send(c);
    }
}

pub fn read_data(bytes: &mut [u8]) -> usize {
    let mut read_len = 0;
    while read_len < bytes.len() {
        if let Some(c) = getchar() {
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
        crate::ConsoleTransport::Mmio { .. } => panic!("ns16550-ioport does not support mmio transport"),
        crate::ConsoleTransport::IoPort { io_port } => init(io_port),
    }
}

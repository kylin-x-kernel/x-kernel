// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! NS16550-compatible serial console implementation for x86 platforms.

use kspin::SpinNoIrq;
use uart_16550::SerialPort;

static COM1: SpinNoIrq<SerialPort> = unsafe { SpinNoIrq::new(SerialPort::new(0x3f8)) };

/// Writes a byte to the serial console.
pub fn putchar(c: u8) {
    COM1.lock().send(c)
}

/// Reads a byte from the serial console, if available.
pub fn getchar() -> Option<u8> {
    COM1.lock().try_receive().ok()
}

/// Initializes the serial console.
pub fn init() {
    COM1.lock().init();
}

/// Implement `kplat::io::ConsoleIf` using the NS16550 serial backend.
///
/// The `irq` argument specifies the optional interrupt ID for the console device.
/// Use `Some(4)` for QEMU virt (COM1 on IRQ 4) or `None` if interrupt-driven
/// console is not needed.
#[allow(clippy::crate_in_macro_def)]
#[macro_export]
macro_rules! console_if_impl {
    ($name:ident,irq = $irq:expr) => {
        struct $name;
        #[impl_dev_interface]
        impl kplat::io::ConsoleIf for $name {
            fn write_data(bytes: &[u8]) {
                for c in bytes {
                    $crate::ns16550::putchar(*c);
                }
            }

            fn read_data(bytes: &mut [u8]) -> usize {
                let mut read_len = 0;
                while read_len < bytes.len() {
                    if let Some(c) = $crate::ns16550::getchar() {
                        bytes[read_len] = c;
                    } else {
                        break;
                    }
                    read_len += 1;
                }
                read_len
            }

            fn interrupt_id() -> Option<usize> {
                $irq
            }
        }
    };
}

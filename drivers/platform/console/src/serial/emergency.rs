// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Best-effort UART transmission without borrowing the normal, locked backend.

#[cfg(feature = "pl011")]
use arm_pl011::Pl011Uart;

#[cfg(feature = "ns16550-mmio")]
use crate::ns16550_mmio::{Port, SerialRegWidth};

// Bound each byte's polling without relying on clocks, IRQs, or another CPU.
const TX_POLL_LIMIT: usize = 100_000;

/// Immutable addresses captured by `SerialPort` after mapping the device.
/// All variants share the existing UART mapping and never reinitialize it.
pub(super) enum EmergencyTx {
    #[cfg(unittest)]
    Test,
    #[cfg(feature = "pl011")]
    Pl011 { base: usize },
    #[cfg(feature = "ns16550-mmio")]
    Ns16550Mmio {
        base: usize,
        stride: usize,
        reg_width: SerialRegWidth,
    },
    #[cfg(all(feature = "ns16550-ioport", target_arch = "x86_64"))]
    Ns16550IoPort { base: u16 },
}

impl EmergencyTx {
    pub(super) fn write_data(&self, bytes: &[u8]) {
        #[cfg(unittest)]
        #[expect(
            clippy::panic,
            reason = "CG-021-no-panic-on-reachable-failure: test scenario: the unittest-only Test \
                      port must never reach emergency output; reaching it fails the test that \
                      wrote to it"
        )]
        if matches!(self, Self::Test) {
            panic!("unexpected emergency output on a test serial port");
        }
        for &byte in bytes {
            let raw_bytes = match byte {
                b'\n' => &b"\r\n"[..],
                8 | 0x7f if self.expands_backspace() => &b"\x08 \x08"[..],
                _ => core::slice::from_ref(&byte),
            };
            for &raw_byte in raw_bytes {
                if !self.send_raw(raw_byte) {
                    return;
                }
            }
        }
    }

    fn expands_backspace(&self) -> bool {
        match self {
            #[cfg(unittest)]
            #[expect(
                clippy::panic,
                reason = "CG-021-no-panic-on-reachable-failure: test scenario: the unittest-only \
                          Test port must never reach emergency output; reaching it fails the test \
                          that wrote to it"
            )]
            Self::Test => panic!("unexpected emergency output on a test serial port"),
            #[cfg(feature = "pl011")]
            Self::Pl011 { .. } => false,
            #[cfg(feature = "ns16550-mmio")]
            Self::Ns16550Mmio { .. } => true,
            #[cfg(all(feature = "ns16550-ioport", target_arch = "x86_64"))]
            Self::Ns16550IoPort { .. } => true,
        }
    }

    fn send_raw(&self, byte: u8) -> bool {
        for _ in 0..TX_POLL_LIMIT {
            if self.try_send_raw(byte) {
                return true;
            }
            core::hint::spin_loop();
        }
        false
    }

    fn try_send_raw(&self, byte: u8) -> bool {
        match self {
            #[cfg(unittest)]
            #[expect(
                clippy::panic,
                reason = "CG-021-no-panic-on-reachable-failure: test scenario: the unittest-only \
                          Test port must never reach emergency output; reaching it fails the test \
                          that wrote to it"
            )]
            Self::Test => panic!("unexpected emergency output on a test serial port"),
            #[cfg(feature = "pl011")]
            Self::Pl011 { base } => {
                // A local handle over the same register window; no reference
                // to the locked `Backend` is created. `try_putchar` only polls
                // FR[TXFF] and writes DR, so it never reinitializes the device.
                let mut uart = Pl011Uart::new(*base as *mut u8);
                uart.try_putchar(byte)
            }
            #[cfg(feature = "ns16550-mmio")]
            Self::Ns16550Mmio {
                base,
                stride,
                reg_width,
            } => {
                // SAFETY: these immutable values came from the same validated
                // mapping/layout as the normal port. This local handle only
                // polls LSR and writes DATA; it never initializes the device.
                let mut port = unsafe { Port::new(*base, *stride, *reg_width) };
                port.try_send_raw(byte).is_ok()
            }
            #[cfg(all(feature = "ns16550-ioport", target_arch = "x86_64"))]
            Self::Ns16550IoPort { base } => {
                // SAFETY: SerialPort's constructor requires a valid UART I/O
                // base. This local handle does not borrow the locked backend
                // or reprogram the device; try_send_raw only accesses LSR/TX.
                let mut port = unsafe { uart_16550::SerialPort::new(*base) };
                port.try_send_raw(byte).is_ok()
            }
        }
    }
}

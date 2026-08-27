// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Minimal 16550A-compatible UART for RISC-V virt guests.

#![no_std]

extern crate alloc;

use alloc::sync::Arc;

use vdev_core::{IrqSender, MmioDevice, RxChannel, TxChannel};

pub const UART_BASE: u64 = 0x1000_0000;
pub const UART_SIZE: u64 = 0x100;

/// Guest PLIC source line this UART is wired to (matches the guest DTB
/// `interrupts = <10>` on the `uart@10000000` node).
pub const UART_IRQ: u32 = 10;

const UART_RBR_THR_DLL: u64 = 0x00;
const UART_IER_DLM: u64 = 0x01;
const UART_IIR_FCR: u64 = 0x02;
const UART_LCR: u64 = 0x03;
const UART_MCR: u64 = 0x04;
const UART_LSR: u64 = 0x05;
const UART_MSR: u64 = 0x06;
const UART_SCR: u64 = 0x07;

const IER_RX_AVAILABLE: u8 = 1 << 0;
const IER_THR_EMPTY: u8 = 1 << 1;
const LSR_DATA_READY: u8 = 1 << 0;
const LSR_THRE: u8 = 1 << 5;
const LSR_TEMT: u8 = 1 << 6;
const IIR_NO_INTERRUPT: u8 = 1 << 0;
/// IIR "interrupt pending" ID for received-data-available (bit0 clear).
const IIR_RX_AVAILABLE: u8 = 0x04;
/// IIR "interrupt pending" ID for transmitter-holding-register empty.
const IIR_THR_EMPTY: u8 = 0x02;
const LINE_BUF_SIZE: usize = 256;

struct Uart16550Inner {
    ier: u8,
    lcr: u8,
    mcr: u8,
    scr: u8,
    dll: u8,
    dlm: u8,
    rx: Arc<RxChannel>,
    tx: Arc<TxChannel>,
    /// Interrupt line into the VM's controller, wired after construction via
    /// [`Uart16550::attach_irq`]. `None` leaves the UART in polled mode.
    irq_sender: Option<Arc<dyn IrqSender>>,
    /// Target vCPU / PLIC context the UART interrupt is routed to.
    irq_target: u32,
    line_buf: [u8; LINE_BUF_SIZE],
    line_len: usize,
}

/// Line-buffered 16550A UART console.
pub struct Uart16550 {
    inner: ksync::Mutex<Uart16550Inner>,
}

/// MMIO front-end for a shared [`Uart16550`] instance.
pub struct Uart16550Mmio {
    uart: Arc<Uart16550>,
}

impl Uart16550Mmio {
    /// Create an MMIO front-end for `uart`.
    pub fn new(uart: Arc<Uart16550>) -> Self {
        Self { uart }
    }
}

impl Uart16550 {
    /// Create a per-VM 16550A instance plus its shared RX and TX channels.
    ///
    /// `vm_id` is accepted for call-site symmetry with other console devices
    /// but is not retained; guest output is emitted without a per-VM prefix.
    ///
    /// The returned [`TxChannel`] starts disabled: until a control device
    /// enables it, guest output stays on the host kernel log.
    pub fn new(_vm_id: u32) -> (Arc<Self>, Arc<RxChannel>, Arc<TxChannel>) {
        let rx = Arc::new(RxChannel::new());
        let tx = Arc::new(TxChannel::new());
        let dev = Self {
            inner: ksync::Mutex::new(Uart16550Inner {
                ier: 0,
                lcr: 0,
                mcr: 0,
                scr: 0,
                dll: 0,
                dlm: 0,
                rx: rx.clone(),
                tx: tx.clone(),
                irq_sender: None,
                irq_target: 0,
                line_buf: [0; LINE_BUF_SIZE],
                line_len: 0,
            }),
        };
        (Arc::new(dev), rx, tx)
    }

    /// Wire this UART to the VM's interrupt controller.
    ///
    /// Once attached, the UART raises [`UART_IRQ`] on `target` whenever its
    /// transmitter-holding-register-empty condition becomes deliverable (the
    /// TX path, since the emulated THR is always empty). The RX interrupt is
    /// raised by the console push path in the VMM, which owns the RX FIFO.
    /// Without a sender the UART stays in polled mode.
    pub fn attach_irq(&self, sender: Arc<dyn IrqSender>, target: u32) {
        let mut inner = self.inner.lock();
        inner.irq_sender = Some(sender);
        inner.irq_target = target;
    }
}

impl Uart16550Inner {
    fn divisor_latch_enabled(&self) -> bool {
        self.lcr & 0x80 != 0
    }

    /// Whether the transmitter interrupt condition is currently asserted.
    ///
    /// The emulated THR drains instantly, so it is always empty; the TX
    /// interrupt is therefore asserted whenever the guest has enabled the
    /// THR-empty interrupt in the IER.
    fn tx_irq_active(&self) -> bool {
        self.ier & IER_THR_EMPTY != 0
    }

    /// Raise [`UART_IRQ`] on the attached controller, if any.
    fn assert_irq(&self) {
        if let Some(sender) = &self.irq_sender {
            sender.inject(self.irq_target, UART_IRQ);
        }
    }

    fn flush_line(&mut self, newline: bool) {
        if self.line_len == 0 {
            return;
        }
        let line =
            core::str::from_utf8(&self.line_buf[..self.line_len]).unwrap_or("<invalid utf8>");
        // A real terminal leaves the cursor on the prompt line so the typed
        // command echoes back on the same line. Only genuine end-of-line
        // output (a `\n` from the guest, or a full buffer) advances the line.
        if newline {
            klogger::kprintln!("{}", line);
        } else {
            klogger::kprint!("{}", line);
        }
        self.line_len = 0;
    }

    fn put_char(&mut self, byte: u8) {
        // Channel mode: forward the byte verbatim (including CR) to the host
        // consumer and skip the host-log line buffer entirely.
        if self.tx.is_enabled() {
            self.tx.push(byte);
            return;
        }
        if byte == b'\r' {
            return;
        }
        if byte == b'\n' {
            self.flush_line(true);
            return;
        }
        if self.line_len >= self.line_buf.len() {
            self.flush_line(true);
        }
        self.line_buf[self.line_len] = byte;
        self.line_len += 1;

        if self.line_ends_with(b"login: ")
            || self.line_ends_with(b"Password: ")
            || self.line_ends_with(b"# ")
            || self.line_ends_with(b"$ ")
        {
            self.flush_line(false);
        }
    }

    fn line_ends_with(&self, suffix: &[u8]) -> bool {
        self.line_len >= suffix.len()
            && self.line_buf[self.line_len - suffix.len()..self.line_len] == *suffix
    }
}

impl MmioDevice for Uart16550Mmio {
    fn name(&self) -> &str {
        "uart16550"
    }

    fn mmio_range(&self) -> (u64, u64) {
        (UART_BASE, UART_SIZE)
    }

    fn read(&self, offset: u64, size: u8) -> u64 {
        let inner = self.uart.inner.lock();
        if size != 1 && size != 4 {
            return 0;
        }

        match offset {
            UART_RBR_THR_DLL if inner.divisor_latch_enabled() => inner.dll as u64,
            UART_RBR_THR_DLL => inner.rx.pop().unwrap_or(0) as u64,
            UART_IER_DLM if inner.divisor_latch_enabled() => inner.dlm as u64,
            UART_IER_DLM => inner.ier as u64,
            UART_IIR_FCR => {
                // Report the highest-priority pending interrupt. RX-data
                // available outranks THR-empty; bit0 clear means "pending".
                if inner.rx.irq_pending() {
                    IIR_RX_AVAILABLE as u64
                } else if inner.tx_irq_active() {
                    IIR_THR_EMPTY as u64
                } else {
                    IIR_NO_INTERRUPT as u64
                }
            }
            UART_LCR => inner.lcr as u64,
            UART_MCR => inner.mcr as u64,
            UART_LSR => {
                let mut lsr = LSR_THRE | LSR_TEMT;
                if inner.rx.has_data() {
                    lsr |= LSR_DATA_READY;
                }
                lsr as u64
            }
            UART_MSR => 0,
            UART_SCR => inner.scr as u64,
            _ => 0,
        }
    }

    fn write(&mut self, offset: u64, size: u8, value: u64) {
        let mut inner = self.uart.inner.lock();
        if size != 1 && size != 4 {
            return;
        }

        let value = value as u8;
        match offset {
            UART_RBR_THR_DLL if inner.divisor_latch_enabled() => inner.dll = value,
            UART_RBR_THR_DLL => {
                inner.put_char(value);
                // THR drained instantly and is empty again; if the guest wants
                // TX interrupts, signal it can send the next byte. Without this
                // an interrupt-driven 8250 driver stalls after the first byte.
                if inner.tx_irq_active() {
                    inner.assert_irq();
                }
            }
            UART_IER_DLM if inner.divisor_latch_enabled() => inner.dlm = value,
            UART_IER_DLM => {
                inner.ier = value;
                inner
                    .rx
                    .set_irq_enabled((inner.ier & IER_RX_AVAILABLE) != 0);
                // Enabling an interrupt whose condition already holds must raise
                // it immediately (level-triggered): THR is always empty, and RX
                // data may already be buffered.
                if inner.tx_irq_active() || inner.rx.irq_pending() {
                    inner.assert_irq();
                }
            }
            UART_IIR_FCR => {}
            UART_LCR => inner.lcr = value,
            UART_MCR => inner.mcr = value,
            UART_SCR => inner.scr = value,
            _ => {}
        }
    }
}

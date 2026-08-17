// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Minimal 16550A-compatible UART for RISC-V virt guests.

use alloc::sync::Arc;

use crate::vdev::MmioDevice;

pub const UART_BASE: u64 = 0x1000_0000;
pub const UART_SIZE: u64 = 0x100;

const UART_RBR_THR_DLL: u64 = 0x00;
const UART_IER_DLM: u64 = 0x01;
const UART_IIR_FCR: u64 = 0x02;
const UART_LCR: u64 = 0x03;
const UART_MCR: u64 = 0x04;
const UART_LSR: u64 = 0x05;
const UART_MSR: u64 = 0x06;
const UART_SCR: u64 = 0x07;

const IER_RX_AVAILABLE: u8 = 1 << 0;
const LSR_DATA_READY: u8 = 1 << 0;
const LSR_THRE: u8 = 1 << 5;
const LSR_TEMT: u8 = 1 << 6;
const IIR_NO_INTERRUPT: u8 = 1 << 0;
const LINE_BUF_SIZE: usize = 256;

/// Line-buffered RISC-V UART console.
pub struct Uart16550 {
    vm_id: u32,
    ier: u8,
    lcr: u8,
    mcr: u8,
    scr: u8,
    dll: u8,
    dlm: u8,
    rx: Arc<vdev_vpl011::RxChannel>,
    line_buf: [u8; LINE_BUF_SIZE],
    line_len: usize,
}

impl Uart16550 {
    pub fn new(vm_id: u32) -> (Self, Arc<vdev_vpl011::RxChannel>) {
        let rx = Arc::new(vdev_vpl011::RxChannel::new());
        let dev = Self {
            vm_id,
            ier: 0,
            lcr: 0,
            mcr: 0,
            scr: 0,
            dll: 0,
            dlm: 0,
            rx: rx.clone(),
            line_buf: [0; LINE_BUF_SIZE],
            line_len: 0,
        };
        (dev, rx)
    }

    fn divisor_latch_enabled(&self) -> bool {
        self.lcr & 0x80 != 0
    }

    fn flush_line(&mut self) {
        if self.line_len == 0 {
            return;
        }
        let line =
            core::str::from_utf8(&self.line_buf[..self.line_len]).unwrap_or("<invalid utf8>");
        khal::kprintln!("[guest {}] {}", self.vm_id, line);
        self.line_len = 0;
    }

    fn put_char(&mut self, byte: u8) {
        if byte == b'\r' {
            return;
        }
        if byte == b'\n' {
            self.flush_line();
            return;
        }
        if self.line_len >= self.line_buf.len() {
            self.flush_line();
        }
        self.line_buf[self.line_len] = byte;
        self.line_len += 1;

        if self.line_ends_with(b"login: ")
            || self.line_ends_with(b"Password: ")
            || self.line_ends_with(b"# ")
            || self.line_ends_with(b"$ ")
        {
            self.flush_line();
        }
    }

    fn line_ends_with(&self, suffix: &[u8]) -> bool {
        self.line_len >= suffix.len()
            && self.line_buf[self.line_len - suffix.len()..self.line_len] == *suffix
    }
}

impl MmioDevice for Uart16550 {
    fn name(&self) -> &str {
        "riscv-uart16550"
    }

    fn mmio_range(&self) -> (u64, u64) {
        (UART_BASE, UART_SIZE)
    }

    fn read(&self, offset: u64, size: u8) -> u64 {
        if size != 1 && size != 4 {
            return 0;
        }

        match offset {
            UART_RBR_THR_DLL if self.divisor_latch_enabled() => self.dll as u64,
            UART_RBR_THR_DLL => self.rx.pop().unwrap_or(0) as u64,
            UART_IER_DLM if self.divisor_latch_enabled() => self.dlm as u64,
            UART_IER_DLM => self.ier as u64,
            UART_IIR_FCR => IIR_NO_INTERRUPT as u64,
            UART_LCR => self.lcr as u64,
            UART_MCR => self.mcr as u64,
            UART_LSR => {
                let mut lsr = LSR_THRE | LSR_TEMT;
                if self.rx.has_data() {
                    lsr |= LSR_DATA_READY;
                }
                lsr as u64
            }
            UART_MSR => 0,
            UART_SCR => self.scr as u64,
            _ => 0,
        }
    }

    fn write(&mut self, offset: u64, size: u8, value: u64) {
        if size != 1 && size != 4 {
            return;
        }

        let value = value as u8;
        match offset {
            UART_RBR_THR_DLL if self.divisor_latch_enabled() => self.dll = value,
            UART_RBR_THR_DLL => self.put_char(value),
            UART_IER_DLM if self.divisor_latch_enabled() => self.dlm = value,
            UART_IER_DLM => {
                self.ier = value;
                self.rx.set_irq_enabled((self.ier & IER_RX_AVAILABLE) != 0);
            }
            UART_IIR_FCR => {}
            UART_LCR => self.lcr = value,
            UART_MCR => self.mcr = value,
            UART_SCR => self.scr = value,
            _ => {}
        }
    }
}

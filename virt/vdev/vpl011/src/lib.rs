// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Virtual PL011 UART — guest console TX/RX.
//!
//! TX is line-buffered and emitted to the host log (`[guest N] ...`). RX flows
//! through a shared [`RxChannel`]: the host `/dev/kvmm` writer pushes bytes and
//! the guest MMIO read path pops them. When RX IRQ is enabled the vGIC can
//! inject the UART interrupt so the guest wakes to drain the FIFO.

#![no_std]

extern crate alloc;

use alloc::sync::Arc;
use core::{
    cell::UnsafeCell,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use vdev_core::MmioDevice;

pub const PL011_BASE: u64 = 0x0900_0000;
pub const PL011_SIZE: u64 = 0x1000;

/// SPI raised by the UART for RX (QEMU virt: PL011 = SPI 1 = IRQ 33).
pub const PL011_IRQ: u32 = 33;

// PL011 register offsets.
const UARTDR: u64 = 0x000;
const UARTFR: u64 = 0x018;
const UARTCR: u64 = 0x030;
const UARTIMSC: u64 = 0x038;
const UARTRIS: u64 = 0x03C;
const UARTMIS: u64 = 0x040;
const UARTICR: u64 = 0x044;

// Peripheral / PrimeCell ID registers (ARM PL011 signature).
const PERIPHID0: u64 = 0xFE0;
const PERIPHID1: u64 = 0xFE4;
const PERIPHID2: u64 = 0xFE8;
const PERIPHID3: u64 = 0xFEC;
const PCELLID0: u64 = 0xFF0;
const PCELLID1: u64 = 0xFF4;
const PCELLID2: u64 = 0xFF8;
const PCELLID3: u64 = 0xFFC;

const FR_TXFE: u32 = 1 << 7; // TX FIFO empty
const FR_RXFE: u32 = 1 << 4; // RX FIFO empty
const INT_RX: u32 = 1 << 4; // RX interrupt (RIS/MIS/IMSC bit 4)

const LINE_BUF_SIZE: usize = 256;
const RX_CAP: usize = 64;

/// Single-producer/single-consumer RX FIFO for one VM's console.
///
/// The host `/dev/kvmm` write path is the sole producer (`push`); the guest
/// vCPU MMIO read path is the sole consumer (`pop`). `head` is advanced only by
/// the consumer, `tail` only by the producer, so the two sides never touch the
/// same slot concurrently.
pub struct RxChannel {
    data: UnsafeCell<[u8; RX_CAP]>,
    head: AtomicUsize,
    tail: AtomicUsize,
    irq_enabled: AtomicBool,
}

// SAFETY: SPSC ring buffer. The producer writes `data[tail]` then advances
// `tail` with Release; the consumer reads `data[head]` then advances `head`
// with Release, observing occupancy via Acquire loads of the opposite index.
// A slot is only accessed when `tail != head`, and each index is mutated by
// exactly one side, so producer and consumer never touch the same slot at once.
unsafe impl Sync for RxChannel {}

impl RxChannel {
    pub fn new() -> Self {
        Self {
            data: UnsafeCell::new([0u8; RX_CAP]),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            irq_enabled: AtomicBool::new(false),
        }
    }

    /// Push a byte into the guest UART RX FIFO. Returns false if full.
    pub fn push(&self, byte: u8) -> bool {
        let tail = self.tail.load(Ordering::Relaxed);
        let next = (tail + 1) % RX_CAP;
        if next == self.head.load(Ordering::Acquire) {
            return false;
        }
        // SAFETY: single producer; consumer cannot read slot `tail` until it
        // observes the Release store of `tail` below.
        unsafe {
            (*self.data.get())[tail] = byte;
        }
        self.tail.store(next, Ordering::Release);
        true
    }

    pub fn pop(&self) -> Option<u8> {
        let head = self.head.load(Ordering::Relaxed);
        if head == self.tail.load(Ordering::Acquire) {
            return None;
        }
        // SAFETY: single consumer; `tail != head` means slot `head` is not the
        // producer's current target.
        let byte = unsafe { (*self.data.get())[head] };
        self.head.store((head + 1) % RX_CAP, Ordering::Release);
        Some(byte)
    }

    pub fn has_data(&self) -> bool {
        self.head.load(Ordering::Relaxed) != self.tail.load(Ordering::Acquire)
    }

    /// True if RX has pending data AND the guest enabled the RX interrupt.
    pub fn irq_pending(&self) -> bool {
        self.has_data() && self.irq_enabled.load(Ordering::Relaxed)
    }

    pub fn set_irq_enabled(&self, enabled: bool) {
        self.irq_enabled.store(enabled, Ordering::Release);
    }
}

impl Default for RxChannel {
    fn default() -> Self {
        Self::new()
    }
}

/// Emulated PL011 UART for one VM.
pub struct Vpl011 {
    vm_id: u32,
    cr: u32,
    imsc: u32,
    rx: Arc<RxChannel>,
    line_buf: [u8; LINE_BUF_SIZE],
    line_len: usize,
}

impl Vpl011 {
    /// Create a per-VM PL011 instance plus the shared RX channel handle (so the
    /// host can route console input without going through the MMIO bus).
    pub fn new(vm_id: u32) -> (Self, Arc<RxChannel>) {
        let rx = Arc::new(RxChannel::new());
        let dev = Self {
            vm_id,
            cr: 0x301, // UARTEN | TXE | RXE
            imsc: 0,
            rx: rx.clone(),
            line_buf: [0; LINE_BUF_SIZE],
            line_len: 0,
        };
        (dev, rx)
    }

    fn flush_line(&mut self) {
        if self.line_len == 0 {
            return;
        }
        let s = core::str::from_utf8(&self.line_buf[..self.line_len]).unwrap_or("<invalid utf8>");
        klogger::kprintln!("[guest {}] {}", self.vm_id, s);
        self.line_len = 0;
    }

    fn put_char(&mut self, c: u8) {
        if c == b'\n' || c == b'\r' {
            self.flush_line();
            return;
        }

        if self.line_len >= LINE_BUF_SIZE {
            self.flush_line();
        }

        self.line_buf[self.line_len] = c;
        self.line_len += 1;

        // Flush eagerly on common prompts that have no trailing newline.
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

impl MmioDevice for Vpl011 {
    fn name(&self) -> &str {
        "vpl011"
    }

    fn mmio_range(&self) -> (u64, u64) {
        (PL011_BASE, PL011_SIZE)
    }

    fn read(&self, offset: u64, _size: u8) -> u64 {
        match offset {
            UARTDR => self.rx.pop().unwrap_or(0) as u64,
            UARTFR => {
                let mut flags = FR_TXFE;
                if !self.rx.has_data() {
                    flags |= FR_RXFE;
                }
                flags as u64
            }
            UARTCR => self.cr as u64,
            UARTIMSC => self.imsc as u64,
            UARTRIS => {
                let ris = if self.rx.has_data() { INT_RX } else { 0 };
                ris as u64
            }
            UARTMIS => {
                let ris = if self.rx.has_data() { INT_RX } else { 0 };
                (ris & self.imsc) as u64
            }
            PERIPHID0 => 0x11,
            PERIPHID1 => 0x10,
            PERIPHID2 => 0x14,
            PERIPHID3 => 0x00,
            PCELLID0 => 0x0D,
            PCELLID1 => 0xF0,
            PCELLID2 => 0x05,
            PCELLID3 => 0xB1,
            _ => 0,
        }
    }

    fn write(&mut self, offset: u64, _size: u8, value: u64) {
        match offset {
            UARTDR => self.put_char(value as u8),
            UARTCR => self.cr = value as u32,
            UARTIMSC => {
                self.imsc = value as u32;
                self.rx.set_irq_enabled((self.imsc & INT_RX) != 0);
            }
            UARTICR => {}
            _ => {}
        }
    }
}

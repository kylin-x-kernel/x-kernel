// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! NS16550-compatible MMIO UART backend.
//!
//! This mirrors the Linux 8250 split at the module level: the generic serial
//! port owns lifetime and role, while this backend owns the register layout and
//! access width selected from device-tree properties such as `reg-shift` and
//! `reg-io-width`.

/// MMIO register access width for NS16550-compatible UARTs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SerialRegWidth {
    /// 8-bit MMIO register access.
    U8,
    /// 16-bit MMIO register access.
    U16,
    /// 32-bit little-endian MMIO register access.
    U32,
}

impl SerialRegWidth {
    /// Decode a device-tree `reg-io-width` byte value.
    pub fn from_bytes(bytes: u32) -> Option<Self> {
        match bytes {
            1 => Some(Self::U8),
            2 => Some(Self::U16),
            4 => Some(Self::U32),
            _ => None,
        }
    }

    fn bytes(self) -> usize {
        match self {
            Self::U8 => 1,
            Self::U16 => 2,
            Self::U32 => 4,
        }
    }
}

pub(crate) struct Port {
    base: usize,
    stride: usize,
    reg_width: SerialRegWidth,
}

impl Port {
    const DATA: usize = 0;
    const FCR: usize = 2;
    const IER: usize = 1;
    const LCR: usize = 3;
    const LCR_DLAB: u8 = 0x80;
    const LSR: usize = 5;
    const LSR_DATA_READY: u8 = 0x01;
    const LSR_TX_EMPTY: u8 = 0x20;
    const MCR: usize = 4;

    /// # Safety
    ///
    /// `base` must name a valid, exclusively-mapped NS16550 MMIO register
    /// window. The register stride and width must match the hardware so each
    /// computed register address is valid and naturally aligned for `reg_width`.
    pub(crate) unsafe fn new(base: usize, stride: usize, reg_width: SerialRegWidth) -> Self {
        debug_assert!(reg_width.bytes() <= stride);
        Self {
            base,
            stride,
            reg_width,
        }
    }

    fn reg_addr(&self, off: usize) -> usize {
        self.base + off * self.stride
    }

    unsafe fn read_reg(&self, off: usize) -> u8 {
        let addr = self.reg_addr(off);
        match self.reg_width {
            SerialRegWidth::U8 => {
                // SAFETY: `Self::new` requires `addr` to name a valid 8-bit
                // MMIO register in this exclusively-mapped UART window.
                unsafe { (addr as *const u8).read_volatile() }
            }
            SerialRegWidth::U16 => {
                // SAFETY: `Self::new` requires `addr` to be valid and aligned
                // for a 16-bit MMIO access to this UART register.
                unsafe { (addr as *const u16).read_volatile() as u8 }
            }
            SerialRegWidth::U32 => {
                // SAFETY: `Self::new` requires `addr` to be valid and aligned
                // for a 32-bit MMIO access to this UART register.
                unsafe { (addr as *const u32).read_volatile() as u8 }
            }
        }
    }

    unsafe fn write_reg(&self, off: usize, val: u8) {
        let addr = self.reg_addr(off);
        match self.reg_width {
            SerialRegWidth::U8 => {
                // SAFETY: `Self::new` requires `addr` to name a valid 8-bit
                // MMIO register in this exclusively-mapped UART window.
                unsafe { (addr as *mut u8).write_volatile(val) }
            }
            SerialRegWidth::U16 => {
                // SAFETY: `Self::new` requires `addr` to be valid and aligned
                // for a 16-bit MMIO access to this UART register.
                unsafe { (addr as *mut u16).write_volatile(val as u16) }
            }
            SerialRegWidth::U32 => {
                // SAFETY: `Self::new` requires `addr` to be valid and aligned
                // for a 32-bit MMIO access to this UART register.
                unsafe { (addr as *mut u32).write_volatile(val as u32) }
            }
        }
    }

    pub(crate) unsafe fn init_preserve_baud(&self) {
        // SAFETY: `self` carries the MMIO window invariants established by
        // `Self::new`; these accesses are to standard NS16550 registers.
        unsafe { self.write_reg(Self::LCR, self.read_reg(Self::LCR) & !Self::LCR_DLAB) };
        // SAFETY: same MMIO register window invariant as above.
        unsafe { self.write_reg(Self::FCR, 0xC7) };
        // SAFETY: same MMIO register window invariant as above.
        unsafe { self.write_reg(Self::MCR, 0x0B) };
        // SAFETY: same MMIO register window invariant as above.
        unsafe { self.write_reg(Self::IER, 0x01) };
    }

    pub(crate) fn send(&mut self, data: u8) {
        match data {
            8 | 0x7f => {
                self.send_raw(8);
                self.send_raw(b' ');
                self.send_raw(8);
            }
            data => self.send_raw(data),
        }
    }

    fn send_raw(&mut self, data: u8) {
        while self.try_send_raw(data).is_err() {
            core::hint::spin_loop();
        }
    }

    fn try_send_raw(&mut self, data: u8) -> Result<(), ()> {
        // SAFETY: `self` carries the MMIO window invariants established by
        // `Self::new`; LSR and DATA are standard NS16550 registers.
        if unsafe { self.read_reg(Self::LSR) } & Self::LSR_TX_EMPTY != 0 {
            // SAFETY: same MMIO register window invariant as above.
            unsafe { self.write_reg(Self::DATA, data) };
            Ok(())
        } else {
            Err(())
        }
    }

    pub(crate) fn try_receive(&mut self) -> Result<u8, ()> {
        // SAFETY: `self` carries the MMIO window invariants established by
        // `Self::new`; LSR and DATA are standard NS16550 registers.
        if unsafe { self.read_reg(Self::LSR) } & Self::LSR_DATA_READY != 0 {
            // SAFETY: same MMIO register window invariant as above.
            Ok(unsafe { self.read_reg(Self::DATA) })
        } else {
            Err(())
        }
    }
}

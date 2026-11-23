// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! NS16550-compatible MMIO UART backend.
//!
//! This mirrors the Linux 8250 split at the module level: the generic serial
//! port owns lifetime and role, while this backend owns the register layout and
//! access width selected from device-tree properties such as `reg-shift` and
//! `reg-io-width`.
//!
//! # MMIO safety model
//!
//! Every access funnels through [`Port`], whose constructor receives one
//! exclusively owned, live MMIO mapping. `stride` and `reg_width` come from
//! firmware and bound every `base + off * stride` register address; `size`
//! bounds the fixed-offset DesignWare `USR` read, which is not derived from
//! `reg-shift`. Ordinary I/O serializes accesses with the `SerialIo` lock, and
//! emergency transmit only polls status and writes data on the same mapping.

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

/// The interrupt source an `IIR` read reports.
///
/// Decoded by the whole identifier nibble, never by `IIR[0]`. On a DesignWare
/// UART the busy-detect code `0x7` has bit 0 set, so the classic 16550 test —
/// "bit 0 set means no interrupt pending" — reads a live interrupt as an idle
/// line. Nothing then reads `USR`, which is the only thing that deasserts it,
/// and a level-triggered GIC input storms until the unclaimed-interrupt
/// watchdog masks it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IirSource {
    /// `0x1`: nothing pending.
    None,
    /// `0x4` / `0xC`: receive data available, or a character timeout.
    /// Deasserted by draining the FIFO through [`Port::try_receive`].
    Receive,
    /// `0x6`: a latched overrun/parity/framing/break error. Deasserted by
    /// reading `LSR`.
    LineStatus,
    /// `0x7`: DesignWare busy detect. Deasserted **only** by reading `USR`.
    BusyDetect,
    /// An identifier this driver neither enables nor services.
    Other,
}

impl IirSource {
    /// Decode an `IIR` read.
    const fn decode(iir: u8) -> Self {
        match iir & Port::IIR_ID_MASK {
            Port::IIR_NONE => Self::None,
            Port::IIR_RX_DATA | Port::IIR_RX_TIMEOUT => Self::Receive,
            Port::IIR_RX_LINE_STATUS => Self::LineStatus,
            Port::IIR_BUSY_DETECT => Self::BusyDetect,
            _ => Self::Other,
        }
    }
}

pub(crate) struct Port {
    base: usize,
    stride: usize,
    reg_width: SerialRegWidth,
    /// Length in bytes of the mapping that starts at `base`.
    ///
    /// Bounds the DesignWare-only `USR` read, whose fixed byte offset is not
    /// derived from `reg-shift` and could otherwise leave the mapped window.
    size: usize,
    /// Whether this is a DesignWare APB UART (`snps,dw-apb-uart`).
    ///
    /// Only DesignWare implements `USR` and raises the busy-detect interrupt;
    /// a plain NS16550 has no register at that byte offset. Reading `USR` on
    /// one would touch an address the UART does not decode — on QEMU's
    /// `ns16550a` that is outside the device's MMIO region, and the boot dies
    /// before the first test runs.
    designware: bool,
}

impl Port {
    const DATA: usize = 0;
    const FCR: usize = 2;
    const IER: usize = 1;
    /// Interrupt Identification Register (read-only; FCR is the same offset on
    /// write).
    const IIR: usize = 2;
    /// DesignWare-only `IIR[3:0] == 0x7`: busy-detect indication.
    ///
    /// Raised when software writes `LCR` while the UART is still busy, which is
    /// exactly what [`Self::init_preserve_baud`] does to clear `DLAB`. It is
    /// **not** gated by `IER`, and the 16550 rule "bit 0 set means no interrupt
    /// pending" does not hold for this code.
    const IIR_BUSY_DETECT: u8 = 0x7;
    /// `IIR[3:0]` interrupt-origin encoding (FIFO-enabled 16550).
    const IIR_ID_MASK: u8 = 0x0F;
    /// `IIR[3:0] == 0x1`: no interrupt is pending.
    const IIR_NONE: u8 = 0x01;
    const IIR_RX_DATA: u8 = 0x04;
    const IIR_RX_LINE_STATUS: u8 = 0x06;
    const IIR_RX_TIMEOUT: u8 = 0x0C;
    const LCR: usize = 3;
    const LCR_DLAB: u8 = 0x80;
    const LSR: usize = 5;
    const LSR_DATA_READY: u8 = 0x01;
    const LSR_TX_EMPTY: u8 = 0x20;
    const MCR: usize = 4;
    /// DesignWare UART Status Register, as a raw byte offset.
    ///
    /// Fixed by the DesignWare register map for every data-bus width, so it is
    /// addressed by byte offset rather than by register index. **Reading it is
    /// the only way to clear a busy-detect interrupt** — draining the FIFO or
    /// reading `LSR` leaves it asserted, which pins a level-triggered GIC input
    /// high and storms the line until the unclaimed-interrupt watchdog masks it.
    const USR_BYTE_OFFSET: usize = 0x7C;

    /// Build a register handle over one mapped UART window.
    ///
    /// # Safety
    ///
    /// The caller must own an exclusive, live mapping of `size` bytes starting
    /// at `base` for the whole lifetime of this port, and must serialize every
    /// register access made through it (the `SerialIo` lock for ordinary I/O;
    /// emergency transmit only polls status and writes data). `stride` must be
    /// the device's `1 << reg-shift` and `reg_width` its `reg-io-width`, so
    /// every `off * stride` register address stays inside the mapping and is
    /// naturally aligned for the configured access width. `designware` may be
    /// true only for a `snps,dw-apb-uart`, whose fixed-offset `USR` register
    /// the backend may then read when the mapping covers it; probing `USR` on
    /// another 16550-compatible part reaches an address it does not decode.
    pub(crate) unsafe fn new(
        base: usize,
        stride: usize,
        reg_width: SerialRegWidth,
        size: usize,
        designware: bool,
    ) -> Self {
        debug_assert!(reg_width.bytes() <= stride);
        Self {
            base,
            stride,
            reg_width,
            size,
            designware,
        }
    }

    /// Whether the mapping covers the DesignWare-only `USR` register.
    ///
    /// `USR` has a fixed byte offset, so unlike the numbered registers its
    /// address is not bounded by `stride`; the mapping length is the only
    /// check that keeps the read from leaving the window.
    fn can_read_usr(&self) -> bool {
        self.designware
            && Self::USR_BYTE_OFFSET
                .checked_add(self.reg_width.bytes())
                .is_some_and(|end| end <= self.size)
    }

    fn reg_addr(&self, off: usize) -> usize {
        self.base + off * self.stride
    }

    /// # Safety
    ///
    /// `off` must be one of this module's register constants, so the computed
    /// `base + off * stride` address and the `reg_width`-byte access lie inside
    /// the live mapping established by `Port::new` and are naturally aligned.
    /// The caller must serialize the access with every other UART user.
    unsafe fn read_reg(&self, off: usize) -> u8 {
        let addr = self.reg_addr(off);
        match self.reg_width {
            SerialRegWidth::U8 => {
                // SAFETY: `new` established a live exclusive mapping covering
                // every register address this module uses, and the caller
                // passes a register constant; the address is therefore inside
                // that mapping for this 8-bit volatile read. The caller's UART
                // serialization excludes a concurrent register user.
                unsafe { (addr as *const u8).read_volatile() }
            }
            SerialRegWidth::U16 => {
                // SAFETY: `new` established a live exclusive mapping and that
                // `stride` is at least two bytes; the caller passes a register
                // constant, so the computed address and its naturally aligned
                // two-byte access stay inside the mapping. The caller's UART
                // serialization excludes a concurrent register user.
                unsafe { (addr as *const u16).read_volatile() as u8 }
            }
            SerialRegWidth::U32 => {
                // SAFETY: `new` established a live exclusive mapping and that
                // `stride` is at least four bytes; the caller passes a register
                // constant, so the computed address and its naturally aligned
                // four-byte access stay inside the mapping. The caller's UART
                // serialization excludes a concurrent register user.
                unsafe { (addr as *const u32).read_volatile() as u8 }
            }
        }
    }

    /// Read a register addressed by raw byte offset.
    ///
    /// Used for the DesignWare-only registers (`USR`), whose byte offset is
    /// fixed by the register map rather than derived from `reg-shift`.
    ///
    /// # Safety
    ///
    /// `byte_offset` must be a fixed offset this module has bounds-checked
    /// against the live mapping established by `Port::new` (see
    /// [`Self::can_read_usr`]); the access must be naturally aligned for
    /// `reg_width`, and the caller must serialize it with other UART users.
    unsafe fn read_reg_bytes(&self, byte_offset: usize) -> u8 {
        debug_assert!(
            self.base.checked_add(byte_offset).is_some(),
            "fixed register offset must not overflow the mapping address"
        );
        debug_assert_eq!(
            (self.base + byte_offset) % self.reg_width.bytes(),
            0,
            "fixed register access must be aligned for the configured width"
        );
        let addr = self.base + byte_offset;
        match self.reg_width {
            // SAFETY: `can_read_usr` checked that the fixed offset plus this
            // one-byte access lies within the live mapping established by
            // `new`; a byte access needs no further alignment. The caller's
            // UART serialization excludes a concurrent register user.
            SerialRegWidth::U8 => unsafe { (addr as *const u8).read_volatile() },
            // SAFETY: `can_read_usr` checked the fixed `USR` offset plus a
            // two-byte access against the mapping size. `USR_BYTE_OFFSET` is
            // two-byte aligned and the mapping base is aligned by `new`, so
            // this read is in-bounds and aligned; the caller's UART
            // serialization excludes a concurrent register user.
            SerialRegWidth::U16 => unsafe { (addr as *const u16).read_volatile() as u8 },
            // SAFETY: `can_read_usr` checked the fixed `USR` offset plus a
            // four-byte access against the mapping size. `USR_BYTE_OFFSET` is
            // four-byte aligned and the mapping base is aligned by `new`, so
            // this read is in-bounds and aligned; the caller's UART
            // serialization excludes a concurrent register user.
            SerialRegWidth::U32 => unsafe { (addr as *const u32).read_volatile() as u8 },
        }
    }

    /// # Safety
    ///
    /// `off` must be one of this module's register constants, so the computed
    /// `base + off * stride` address and the `reg_width`-byte access lie inside
    /// the live mapping established by `Port::new` and are naturally aligned.
    /// The caller must serialize the access with every other UART user.
    unsafe fn write_reg(&self, off: usize, val: u8) {
        let addr = self.reg_addr(off);
        match self.reg_width {
            SerialRegWidth::U8 => {
                // SAFETY: `new` established a live exclusive mapping covering
                // every register address this module uses, and the caller
                // passes a register constant; the address is therefore inside
                // that mapping for this 8-bit volatile write. The caller's UART
                // serialization excludes a concurrent register user.
                unsafe { (addr as *mut u8).write_volatile(val) }
            }
            SerialRegWidth::U16 => {
                // SAFETY: `new` established a live exclusive mapping and that
                // `stride` is at least two bytes; the caller passes a register
                // constant, so the computed address and its naturally aligned
                // two-byte access stay inside the mapping. The caller's UART
                // serialization excludes a concurrent register user.
                unsafe { (addr as *mut u16).write_volatile(val as u16) }
            }
            SerialRegWidth::U32 => {
                // SAFETY: `new` established a live exclusive mapping and that
                // `stride` is at least four bytes; the caller passes a register
                // constant, so the computed address and its naturally aligned
                // four-byte access stay inside the mapping. The caller's UART
                // serialization excludes a concurrent register user.
                unsafe { (addr as *mut u32).write_volatile(val as u32) }
            }
        }
    }

    /// # Safety
    ///
    /// The caller must still own the live, exclusively mapped port and must
    /// exclude every other UART access while this runs; it changes the
    /// divisor-latch selector, interrupt mask, FIFO and modem-control registers.
    pub(crate) unsafe fn init_preserve_baud(&self) {
        // SAFETY: `new` established a live exclusive mapping covering LCR at
        // the configured stride and width; the caller's exclusive
        // initialization is what serializes this read against other UART users.
        let lcr = unsafe { self.read_reg(Self::LCR) };
        // SAFETY: the mapping covers LCR at the configured width under `new`'s
        // contract, and the caller's exclusive initialization serializes the
        // write. It only clears DLAB, leaving the divisor and line format.
        unsafe { self.write_reg(Self::LCR, lcr & !Self::LCR_DLAB) };
        // SAFETY: the preceding write cleared DLAB, so register 1 now selects
        // IER rather than the divisor high byte. The address is inside the
        // mapping established by `new`, serialized by exclusive initialization.
        unsafe { self.write_reg(Self::IER, 0) };
        // SAFETY: register 2 is inside the mapping established by `new` and is
        // FCR on write; exclusive initialization serializes the access.
        unsafe { self.write_reg(Self::FCR, 0xC7) };
        // SAFETY: register 4 is inside the mapping established by `new` and is
        // MCR on write; exclusive initialization serializes the access.
        unsafe { self.write_reg(Self::MCR, 0x0B) };
        // The `LCR` write above clears `DLAB`; on a DesignWare UART a write to
        // `LCR` while the transmitter is still draining raises the busy-detect
        // interrupt. Reading `USR` clears it. Skipping this leaves the line
        // asserted from the first byte of console init, and since the input
        // handler cannot clear a condition it does not know about, the
        // level-triggered GIC input storms until the unclaimed-interrupt
        // watchdog masks it — taking console input down with it.
        //
        // Only DesignWare has `USR`; a plain NS16550 does not decode that
        // byte offset, so it must not be read there. `can_read_usr` also keeps
        // the fixed offset inside the mapped window.
        if self.can_read_usr() {
            // SAFETY: `can_read_usr` verified the DesignWare mapping covers the
            // fixed `USR` offset for the configured access width; exclusive
            // initialization serializes this access with other UART users.
            let _usr = unsafe { self.read_reg_bytes(Self::USR_BYTE_OFFSET) };
        }
    }

    pub(crate) fn enable_receive_interrupt(&mut self) {
        // SAFETY: `new` established the live mapping, and initialization
        // cleared DLAB and this backend never sets it again, so register 1 is
        // IER; the mutable borrow and the owning `SerialPort`'s lock serialize
        // this write with other register users.
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

    pub(crate) fn try_send_raw(&mut self, data: u8) -> Result<(), ()> {
        // SAFETY: `new` established the live mapping covering LSR at the
        // configured stride and width; the mutable borrow keeps this polled
        // transmit path serialized with other users of the same port.
        if unsafe { self.read_reg(Self::LSR) } & Self::LSR_TX_EMPTY != 0 {
            // SAFETY: the transmitter is empty and DATA (register 0) is inside
            // the mapping established by `new`; the mutable borrow excludes a
            // concurrent register user for this write.
            unsafe { self.write_reg(Self::DATA, data) };
            Ok(())
        } else {
            Err(())
        }
    }

    pub(crate) fn try_receive(&mut self) -> Result<u8, ()> {
        // SAFETY: `new` established the live mapping covering LSR at the
        // configured stride and width; the mutable borrow keeps this polled
        // receive path serialized with other users of the same port.
        if unsafe { self.read_reg(Self::LSR) } & Self::LSR_DATA_READY != 0 {
            // SAFETY: LSR reported a byte and DATA (register 0) is inside the
            // mapping established by `new`; the mutable borrow excludes a
            // concurrent register user for this read.
            Ok(unsafe { self.read_reg(Self::DATA) })
        } else {
            Err(())
        }
    }

    /// Acknowledge this UART's pending interrupt, reporting whether one was
    /// serviced.
    ///
    /// A 16550 has no "clear all interrupts" register: the interrupt line
    /// deasserts only when its source is *serviced*. `IIR` reports the
    /// highest-priority pending source without clearing anything, so this
    /// decodes it and services the sources that carry no byte:
    ///
    /// * receive data / character timeout — drained by the caller through
    ///   [`Self::try_receive`];
    /// * receiver line status — a latched overrun/parity/framing/break error,
    ///   cleared **only** by the `LSR` read below;
    /// * busy detect on a DesignWare UART — cleared **only** by the `USR` read
    ///   below; a plain NS16550 has no such register and raises no such code.
    ///
    /// A handler that can only drain the FIFO deasserts none of the latter two,
    /// and a source left asserted on a level-triggered GIC input is an
    /// unbounded storm.
    ///
    /// # Context
    ///
    /// Called by the port's input action while the `SerialIo` lock is held. It
    /// performs register reads only: allocation-free, must not sleep, and may
    /// run in hardirq context. Concurrent UART access is excluded by the port
    /// lock; callers outside it must provide the same exclusion.
    pub(crate) fn ack_interrupt(&mut self) -> bool {
        // SAFETY: `new` established the live mapping covering IIR at the
        // configured stride and width; the mutable borrow is the port-lock
        // serialization documented in the function contract, so this read
        // cannot race another register user.
        let iir = unsafe { self.read_reg(Self::IIR) };
        // SAFETY: `new` established the live mapping covering LSR at the
        // configured stride and width, and the same port-lock serialization
        // excludes other users. Reading LSR is what clears the latched
        // receiver-line-status error bits.
        let _lsr = unsafe { self.read_reg(Self::LSR) };
        match IirSource::decode(iir) {
            IirSource::BusyDetect if self.can_read_usr() => {
                // SAFETY: `can_read_usr` verified the DesignWare mapping covers
                // the fixed `USR` offset for the configured access width, and
                // the port-lock serialization excludes other register users.
                let _usr = unsafe { self.read_reg_bytes(Self::USR_BYTE_OFFSET) };
                true
            }
            IirSource::Receive | IirSource::LineStatus => true,
            // Nothing pending, or a source this backend does not enable. Report
            // it as unserviced rather than pretend to clear it, so the
            // unclaimed-interrupt watchdog still catches what this backend
            // cannot name. A busy-detect indication on a non-DesignWare port —
            // or on one whose mapping would not cover `USR` — is in the same
            // class: there is no readable `USR` and nothing this driver can
            // deassert.
            IirSource::None | IirSource::BusyDetect | IirSource::Other => false,
        }
    }
}

unittest::path_mod_test!(tests, "../ut-tests/ns16550_mmio.rs");

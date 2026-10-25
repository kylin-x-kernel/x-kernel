use core::fmt;

use crate::{LineStsFlags, WouldBlockError};

/// A x86 I/O port-mapped UART.
#[cfg_attr(docsrs, doc(cfg(any(target_arch = "x86", target_arch = "x86_64"))))]
#[derive(Debug)]
pub struct SerialPort(u16 /* base port */);

impl SerialPort {
    /// Base port.
    fn port_base(&self) -> u16 {
        self.0
    }

    /// Data port.
    ///
    /// Read and write.
    fn port_data(&self) -> u16 {
        self.port_base()
    }

    /// Interrupt enable port.
    ///
    /// Write only.
    fn port_int_en(&self) -> u16 {
        self.port_base() + 1
    }

    /// Fifo control port.
    ///
    /// Write only.
    fn port_fifo_ctrl(&self) -> u16 {
        self.port_base() + 2
    }

    /// Line control port.
    ///
    /// Write only.
    fn port_line_ctrl(&self) -> u16 {
        self.port_base() + 3
    }

    /// Modem control port.
    ///
    /// Write only.
    fn port_modem_ctrl(&self) -> u16 {
        self.port_base() + 4
    }

    /// Line status port.
    ///
    /// Read only.
    fn port_line_sts(&self) -> u16 {
        self.port_base() + 5
    }

    /// Creates a new serial port interface on the given I/O base port.
    ///
    /// This function is unsafe because the caller must ensure that the given base address
    /// really points to a serial port device and that the caller has the necessary rights
    /// to perform the I/O operation.
    pub const unsafe fn new(base: u16) -> Self {
        Self(base)
    }

    /// Initializes the serial port.
    ///
    /// The default configuration of [38400/8-N-1](https://en.wikipedia.org/wiki/8-N-1) is used.
    pub fn init(&mut self) {
        self.init_masked();
        self.set_receive_interrupt(true);
    }

    /// Initialize 38400/8-N-1 polled I/O, keeping UART interrupts masked.
    ///
    /// As with `init`, this clears the FIFO during setup. Install a handler
    /// before calling `set_receive_interrupt(true)`; do not reinitialize when
    /// merely enabling notification because that would discard received bytes.
    pub fn init_masked(&mut self) {
        // SAFETY: constructing this port requires exclusive I/O ownership and
        // permission; initialization uses only this UART's register offsets.
        self.init_masked_with(|port, value| unsafe { x86::io::outb(port, value) });
    }

    fn init_masked_with(&self, mut write: impl FnMut(u16, u8)) {
        // Firmware may leave DLAB set: select IER before masking notifications.
        write(self.port_line_ctrl(), 0x03);
        write(self.port_int_en(), 0x00);
        write(self.port_line_ctrl(), 0x80);
        write(self.port_data(), 0x03);
        write(self.port_int_en(), 0x00);
        write(self.port_line_ctrl(), 0x03);
        write(self.port_fifo_ctrl(), 0xc7);
        write(self.port_modem_ctrl(), 0x0b);
    }

    /// Enable or mask RX notification without clearing the FIFO.
    ///
    /// Call after initialization, with exclusive access to the UART. DLAB must
    /// remain clear; this driver changes it only inside initialization.
    pub fn set_receive_interrupt(&mut self, enabled: bool) {
        // SAFETY: this initialized port owns its I/O range and has DLAB clear.
        unsafe { x86::io::outb(self.port_int_en(), u8::from(enabled)) };
    }

    fn line_sts(&mut self) -> LineStsFlags {
        unsafe { LineStsFlags::from_bits_truncate(x86::io::inb(self.port_line_sts())) }
    }

    /// Sends a byte on the serial port.
    /// 0x08 (backspace) and 0x7F (delete) get replaced with 0x08, 0x20, 0x08 and 0x0A (\n) gets replaced with \r\n.
    /// If this replacement is unwanted use [SerialPort::send_raw] instead.
    pub fn send(&mut self, data: u8) {
        match data {
            8 | 0x7F => {
                self.send_raw(8);
                self.send_raw(b' ');
                self.send_raw(8);
            }
            0x0A => {
                self.send_raw(0x0D);
                self.send_raw(0x0A);
            }
            data => {
                self.send_raw(data);
            }
        }
    }

    /// Sends a raw byte on the serial port, intended for binary data.
    pub fn send_raw(&mut self, data: u8) {
        retry_until_ok!(self.try_send_raw(data))
    }

    /// Tries to send a raw byte on the serial port, intended for binary data.
    pub fn try_send_raw(&mut self, data: u8) -> Result<(), WouldBlockError> {
        if self.line_sts().contains(LineStsFlags::OUTPUT_EMPTY) {
            unsafe {
                x86::io::outb(self.port_data(), data);
            }
            Ok(())
        } else {
            Err(WouldBlockError)
        }
    }

    /// Receives a byte on the serial port.
    pub fn receive(&mut self) -> u8 {
        retry_until_ok!(self.try_receive())
    }

    /// Tries to receive a byte on the serial port.
    pub fn try_receive(&mut self) -> Result<u8, WouldBlockError> {
        if self.line_sts().contains(LineStsFlags::INPUT_FULL) {
            let data = unsafe { x86::io::inb(self.port_data()) };
            Ok(data)
        } else {
            Err(WouldBlockError)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masked_setup_preserves_configuration_without_rx_enable() {
        // Private fixture: only the injected write sink is used, never port I/O.
        let uart = SerialPort(0x3f8);
        let expected = [
            (0x3fb, 0x03),
            (0x3f9, 0x00),
            (0x3fb, 0x80),
            (0x3f8, 0x03),
            (0x3f9, 0x00),
            (0x3fb, 0x03),
            (0x3fa, 0xc7),
            (0x3fc, 0x0b),
        ];
        let mut expected = expected.iter();
        uart.init_masked_with(|port, value| assert_eq!(Some(&(port, value)), expected.next()));
        assert_eq!(expected.next(), None);
    }
}

impl fmt::Write for SerialPort {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            self.send(byte);
        }
        Ok(())
    }
}

use core::ptr::NonNull;

use tock_registers::{
    interfaces::{Readable, Writeable},
    register_structs,
    registers::{ReadOnly, ReadWrite, WriteOnly},
};

register_structs! {
    /// Pl011 registers.
    Pl011UartRegs {
        /// Data Register.
        (0x00 => dr: ReadWrite<u32>),
        (0x04 => _reserved0),
        /// Flag Register.
        (0x18 => fr: ReadOnly<u32>),
        (0x1c => _reserved1),
        /// Control register.
        (0x30 => cr: ReadWrite<u32>),
        /// Interrupt FIFO Level Select Register.
        (0x34 => ifls: ReadWrite<u32>),
        /// Interrupt Mask Set Clear Register.
        (0x38 => imsc: ReadWrite<u32>),
        /// Raw Interrupt Status Register.
        (0x3c => ris: ReadOnly<u32>),
        /// Masked Interrupt Status Register.
        (0x40 => mis: ReadOnly<u32>),
        /// Interrupt Clear Register.
        (0x44 => icr: WriteOnly<u32>),
        (0x48 => @END),
    }
}

/// The Pl011 Uart
///
/// The Pl011 Uart provides a programing interface for:
/// 1. Construct a new Pl011 UART instance
/// 2. Initialize the Pl011 UART
/// 3. Read a char from the UART
/// 4. Write a char to the UART
/// 5. Handle a UART IRQ
pub struct Pl011Uart {
    base: NonNull<Pl011UartRegs>,
}

unsafe impl Send for Pl011Uart {}
unsafe impl Sync for Pl011Uart {}

#[derive(Debug, PartialEq, Eq)]
enum InitRegister {
    InterruptMask,
    InterruptClear,
    FifoLevel,
    Control,
}

fn init_masked_with(mut write: impl FnMut(InitRegister, u32)) {
    write(InitRegister::InterruptMask, 0);
    write(InitRegister::InterruptClear, 0x7ff);
    write(InitRegister::FifoLevel, 0);
    write(InitRegister::Control, (1 << 0) | (1 << 8) | (1 << 9));
}

impl Pl011Uart {
    /// Constrcut a new Pl011 UART instance from the base address.
    pub const fn new(base: *mut u8) -> Self {
        Self {
            base: NonNull::new(base).unwrap().cast(),
        }
    }

    const fn regs(&self) -> &Pl011UartRegs {
        unsafe { self.base.as_ref() }
    }

    /// Initializes the Pl011 UART.
    ///
    /// It clears all irqs, sets fifo trigger level, enables rx interrupt, enables receives
    pub fn init(&mut self) {
        self.init_masked();
        self.set_receive_interrupt(true);
    }

    /// Initialize polled RX/TX with all UART interrupts masked throughout setup.
    ///
    /// The owner must install its handler before enabling receive interrupts.
    /// This does not change the baud rate or flush the receive FIFO.
    pub fn init_masked(&mut self) {
        init_masked_with(|register, value| match register {
            InitRegister::InterruptMask => self.regs().imsc.set(value),
            InitRegister::InterruptClear => self.regs().icr.set(value),
            InitRegister::FifoLevel => self.regs().ifls.set(value),
            InitRegister::Control => self.regs().cr.set(value),
        });
    }

    /// Enable or mask RX notification without clearing pending input.
    ///
    /// Call after initialization, under the port owner's serialization. Other
    /// interrupt classes remain masked, as in this driver's original `init`.
    pub fn set_receive_interrupt(&mut self, enabled: bool) {
        self.regs().imsc.set(if enabled { 1 << 4 } else { 0 });
    }

    /// Output a char c to data register, waiting until the TX FIFO has space.
    pub fn putchar(&mut self, c: u8) {
        while !self.try_putchar(c) {}
    }

    /// Return `true` if the TX FIFO is full (`FR` bit `TXFF` is set), i.e.
    /// [`Self::putchar`] would block.
    pub fn tx_fifo_full(&self) -> bool {
        self.regs().fr.get() & (1 << 5) != 0
    }

    /// Output a char c to data register without blocking.
    ///
    /// Return `true` if the TX FIFO had space and `c` was written, or `false`
    /// if the TX FIFO was full and nothing was sent (the caller may retry
    /// later). Useful in contexts where blocking indefinitely is unacceptable,
    /// such as interrupt handlers or emergency console output.
    pub fn try_putchar(&mut self, c: u8) -> bool {
        if self.tx_fifo_full() {
            false
        } else {
            self.regs().dr.set(c as u32);
            true
        }
    }

    /// Return a byte if pl011 has received, or it will return `None`.
    pub fn getchar(&mut self) -> Option<u8> {
        if self.regs().fr.get() & (1 << 4) == 0 {
            Some(self.regs().dr.get() as u8)
        } else {
            None
        }
    }

    /// Return true if pl011 has received an interrupt
    pub fn is_receive_interrupt(&self) -> bool {
        let pending = self.regs().mis.get();
        pending & (1 << 4) != 0
    }

    /// Clear all interrupts
    pub fn ack_interrupts(&mut self) {
        self.regs().icr.set(0x7ff);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masked_initialization_never_enables_an_interrupt() {
        let expected = [
            (InitRegister::InterruptMask, 0),
            (InitRegister::InterruptClear, 0x7ff),
            (InitRegister::FifoLevel, 0),
            (InitRegister::Control, 0x301),
        ];
        let mut expected = expected.into_iter();
        init_masked_with(|register, value| assert_eq!(Some((register, value)), expected.next()));
        assert_eq!(expected.next(), None);
    }

    #[test]
    fn receive_activation_preserves_data_and_configuration() {
        let mut regs = [0u32; REGS_WORDS];
        regs[0x38 / 4] = 0x7ff;
        regs[DR_WORD] = 0x41;
        let mut uart = uart_at(&mut regs);
        uart.init_masked();
        assert_eq!(regs[0x38 / 4], 0);
        let prepared = regs;
        uart.set_receive_interrupt(true);
        assert_eq!(regs[0x38 / 4], 1 << 4);
        uart.set_receive_interrupt(false);
        assert_eq!(regs, prepared);
        uart.init();
        assert_eq!(regs[0x38 / 4], 1 << 4);
    }

    /// Size of the register block in 32-bit words: `@END` is at offset 0x48.
    const REGS_WORDS: usize = 0x48 / size_of::<u32>();

    /// FR (Flag Register) word index, at offset 0x18.
    const FR_WORD: usize = 0x18 / size_of::<u32>();
    /// DR (Data Register) word index, at offset 0x00.
    const DR_WORD: usize = 0x00 / size_of::<u32>();
    const FR_TXFF: u32 = 1 << 5;

    fn uart_at(regs: &mut [u32; REGS_WORDS]) -> Pl011Uart {
        Pl011Uart::new(regs.as_mut_ptr() as *mut u8)
    }

    #[test]
    fn tx_fifo_full_follows_fr_txff() {
        let mut regs = [0u32; REGS_WORDS];
        let uart = uart_at(&mut regs);
        assert!(!uart.tx_fifo_full());
        regs[FR_WORD] = FR_TXFF;
        assert!(uart.tx_fifo_full());
    }

    #[test]
    fn try_putchar_writes_when_fifo_has_space() {
        let mut regs = [0u32; REGS_WORDS];
        let mut uart = uart_at(&mut regs);
        assert!(uart.try_putchar(b'A'));
        assert_eq!(regs[DR_WORD], u32::from(b'A'));
    }

    #[test]
    fn try_putchar_fails_without_writing_when_fifo_full() {
        let mut regs = [0u32; REGS_WORDS];
        regs[FR_WORD] = FR_TXFF;
        let mut uart = uart_at(&mut regs);
        assert!(!uart.try_putchar(b'A'));
        assert_eq!(regs[DR_WORD], 0);
    }

    #[test]
    fn putchar_writes_when_fifo_has_space() {
        let mut regs = [0u32; REGS_WORDS];
        let mut uart = uart_at(&mut regs);
        uart.putchar(b'A');
        assert_eq!(regs[DR_WORD], u32::from(b'A'));
    }
}

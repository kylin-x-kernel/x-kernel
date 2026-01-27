use kplat::io::Terminal;
use kspin::SpinNoIrq;
use lazyinit::LazyInit;
use uart_16550::MmioSerialPort;

use crate::config::{devices::UART_PADDR, plat::PHYS_VIRT_OFFSET};
static UART: LazyInit<SpinNoIrq<MmioSerialPort>> = LazyInit::new();
pub(crate) fn early_init() {
    UART.init_once({
        let mut uart = unsafe { MmioSerialPort::new(UART_PADDR + PHYS_VIRT_OFFSET) };
        uart.init();
        SpinNoIrq::new(uart)
    });
}
struct TerminalImpl;
#[impl_dev_interface]
impl Terminal for TerminalImpl {
    fn write_data(bytes: &[u8]) {
        for &c in bytes {
            let mut uart = UART.lock();
            match c {
                b'\n' => {
                    uart.send_raw(b'\r');
                    uart.send_raw(b'\n');
                }
                c => uart.send_raw(c),
            }
        }
    }

    fn read_data(bytes: &mut [u8]) -> usize {
        let mut uart = UART.lock();
        for (i, byte) in bytes.iter_mut().enumerate() {
            match uart.try_receive() {
                Ok(c) => *byte = c,
                Err(_) => return i,
            }
        }
        bytes.len()
    }

    #[cfg(feature = "irq")]
    fn interrupt_id() -> Option<usize> {
        Some(crate::config::devices::UART_IRQ)
    }
}

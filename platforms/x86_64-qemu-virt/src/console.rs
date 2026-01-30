//! Serial console implementation for x86_64-qemu-virt.

use kplat::io::ConsoleIf;
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
struct ConsoleImpl;
#[impl_dev_interface]
impl ConsoleIf for ConsoleImpl {
    fn write_data(bytes: &[u8]) {
        for c in bytes {
            putchar(*c);
        }
    }

    fn read_data(bytes: &mut [u8]) -> usize {
        let mut read_len = 0;
        while read_len < bytes.len() {
            if let Some(c) = getchar() {
                bytes[read_len] = c;
            } else {
                break;
            }
            read_len += 1;
        }
        read_len
    }

    fn interrupt_id() -> Option<usize> {
        None
    }
}

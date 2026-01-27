use kplat::memory::VirtAddr;
use kspin::SpinNoIrq;
use lazyinit::LazyInit;
use uart_16550::{MmioSerialPort, WouldBlockError};
static UART: LazyInit<SpinNoIrq<MmioSerialPort>> = LazyInit::new();
fn do_putchar(uart: &mut MmioSerialPort, c: u8) {
    match c {
        b'\n' => {
            uart.send(b'\r');
            uart.send(b'\n');
        }
        c => uart.send(c),
    }
}
pub fn write_data_force(uart_base: VirtAddr, bytes: &[u8]) {
    let base_addr = uart_base.as_usize();
    let mut uart = unsafe { MmioSerialPort::new(base_addr) };
    uart.init();
    for c in bytes {
        do_putchar(&mut uart, *c);
    }
}
pub fn putchar(c: u8) {
    do_putchar(&mut UART.lock(), c);
}
pub fn getchar<E>() -> Result<u8, WouldBlockError> {
    UART.lock().try_receive()
}
pub fn write_data(bytes: &[u8]) {
    let mut uart = UART.lock();
    for c in bytes {
        do_putchar(&mut uart, *c);
    }
}
pub fn read_data(bytes: &mut [u8]) -> usize {
    let mut read_len = 0;
    while read_len < bytes.len() {
        if let Ok(c) = getchar::<WouldBlockError>() {
            bytes[read_len] = c;
            read_len += 1;
        } else {
            break;
        }
    }
    read_len
}
pub fn early_init(uart_base: VirtAddr) {
    UART.init_once(SpinNoIrq::new({
        let base_addr = uart_base.as_usize();
        let mut uart = unsafe { MmioSerialPort::new(base_addr) };
        uart.init();
        uart
    }));
}
#[allow(clippy::crate_in_macro_def)]
#[macro_export]
macro_rules! ns16550_console_if_impl {
    ($name:ident) => {
        struct $name;
        #[kplat::impl_dev_interface]
        impl kplat::io::Terminal for $name {
            fn write_data(bytes: &[u8]) {
                $crate::ns16550a::write_data(bytes);
            }

            fn write_data_force(bytes: &[u8]) {
                let mut uart_base =
                    kplat::memory::p2v(kplat::memory::pa!(crate::config::devices::UART_PADDR));
                $crate::ns16550a::write_data_force(uart_16550, bytes);
            }

            fn read_data(bytes: &mut [u8]) -> usize {
                $crate::ns16550a::read_data(bytes)
            }

            #[cfg(feature = "irq")]
            fn interrupt_id() -> Option<usize> {
                Some(crate::config::devices::UART_IRQ as _)
            }
        }
    };
}

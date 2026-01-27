use arm_pl011::Pl011Uart;
use kplat::memory::VirtAddr;
use kspin::SpinNoIrq;
use lazyinit::LazyInit;
static UART: LazyInit<SpinNoIrq<Pl011Uart>> = LazyInit::new();
fn do_putchar(uart: &mut Pl011Uart, c: u8) {
    match c {
        b'\n' => {
            uart.putchar(b'\r');
            uart.putchar(b'\n');
        }
        c => uart.putchar(c),
    }
}
pub fn write_data_force(uart_base: VirtAddr, bytes: &[u8]) {
    let mut uart = Pl011Uart::new(uart_base.as_mut_ptr());
    uart.init();
    for c in bytes {
        do_putchar(&mut uart, *c);
    }
}
pub fn putchar(c: u8) {
    do_putchar(&mut UART.lock(), c);
}
pub fn getchar() -> Option<u8> {
    UART.lock().getchar()
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
        if let Some(c) = getchar() {
            bytes[read_len] = c;
        } else {
            break;
        }
        read_len += 1;
    }
    read_len
}
pub fn early_init(uart_base: VirtAddr) {
    UART.init_once(SpinNoIrq::new({
        let mut uart = Pl011Uart::new(uart_base.as_mut_ptr());
        uart.init();
        uart
    }));
}
#[macro_export]
#[allow(clippy::crate_in_macro_def)]
macro_rules! console_if_impl {
    ($name:ident) => {
        struct $name;
        #[kplat::impl_dev_interface]
        impl kplat::io::Terminal for $name {
            fn write_data(bytes: &[u8]) {
                $crate::pl011::write_data(bytes);
            }

            fn write_data_atomic(bytes: &[u8]) {
                let uart_base =
                    kplat::memory::p2v(kplat::memory::pa!(crate::config::devices::UART_PADDR));
                $crate::pl011::write_data_force(uart_base, bytes);
            }

            fn read_data(bytes: &mut [u8]) -> usize {
                $crate::pl011::read_data(bytes)
            }

            #[cfg(feature = "irq")]
            fn interrupt_id() -> Option<usize> {
                Some(crate::config::devices::UART_IRQ as _)
            }
        }
    };
}

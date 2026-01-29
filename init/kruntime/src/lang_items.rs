use core::panic::PanicInfo;

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    kprintln!("{}", info);
    kprintln!("{}", backtrace::Backtrace::capture());
    khal::power::shutdown()
}

use core::panic::PanicInfo;

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    kprintln!("{}", info);
    kprintln!("{}", axbacktrace::Backtrace::capture());
    axhal::power::system_off()
}

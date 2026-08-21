// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Panic handler for the runtime.
use core::panic::PanicInfo;

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    kprintln!("{}", info);
    kprintln!("{}", backtrace::Backtrace::capture());
    // Use the bare platform terminal: the SMP stop hook exchanges IPIs and
    // can deadlock here when this CPU holds a lock that the other CPUs spin
    // on with interrupts disabled. Power-off behaviour matches the
    // pre-halt/power-off split shutdown() path.
    khal::power::platform_power_off()
}

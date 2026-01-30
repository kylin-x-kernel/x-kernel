<<<<<<< HEAD
//! Panic handler for the runtime.
=======
// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

>>>>>>> 62a4f63a (./init, io, mm, net, platforms, process, sync over)
use core::panic::PanicInfo;

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    kprintln!("{}", info);
    kprintln!("{}", backtrace::Backtrace::capture());
    khal::power::shutdown()
}

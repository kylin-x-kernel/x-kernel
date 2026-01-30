// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kplat::timer::{GlobalTimer, NS_SEC};
use riscv::register::time;
const NANOS_PER_TICK: u64 = NS_SEC / crate::config::devices::TIMER_FREQUENCY as u64;
static mut RTC_EPOCHOFFSET_NANOS: u64 = 0;
pub(super) fn early_init() {
    #[cfg(feature = "rtc")]
    use crate::config::{devices::RTC_PADDR, plat::PHYS_VIRT_OFFSET};
    #[cfg(feature = "rtc")]
    if RTC_PADDR != 0 {
        use riscv_goldfish::Rtc;
        let epoch_time_nanos =
            Rtc::new(RTC_PADDR + PHYS_VIRT_OFFSET).get_unix_timestamp() * 1_000_000_000;
        unsafe {
            RTC_EPOCHOFFSET_NANOS =
                epoch_time_nanos - GlobalTimerImpl::t2ns(GlobalTimerImpl::now_ticks());
        }
    }
}
pub(super) fn init_percpu() {
    sbi_rt::set_timer(0);
}
struct GlobalTimerImpl;
#[impl_dev_interface]
impl GlobalTimer for GlobalTimerImpl {
    fn now_ticks() -> u64 {
        time::read() as u64
    }

    fn t2ns(ticks: u64) -> u64 {
        ticks * NANOS_PER_TICK
    }

    fn ns2t(nanos: u64) -> u64 {
        nanos / NANOS_PER_TICK
    }

    fn offset_ns() -> u64 {
        unsafe { RTC_EPOCHOFFSET_NANOS }
    }

    fn freq() -> u64 {
        crate::config::devices::TIMER_FREQUENCY as u64
    }

    fn interrupt_id() -> usize {
        crate::config::devices::TIMER_IRQ
    }

    fn arm_timer(deadline_ns: u64) {
        sbi_rt::set_timer(Self::ns2t(deadline_ns));
    }
}

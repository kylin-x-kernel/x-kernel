// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kbuild_config::RTC_PADDR;
use khal::time::{MonotonicTimerIf, NANOS_PER_MILLIS, NANOS_PER_SEC};
use lazyinit::LazyInit;
use loongArch64::{register::tcfg, time::Time};

const TIMER_IRQ: usize = 11;
const MIN_TIMER_TICKS: u64 = 4;

static NANOS_PER_TICK: LazyInit<u64> = LazyInit::new();

#[inline]
fn now_ticks() -> u64 {
    Time::read() as _
}

#[inline]
fn t2ns(ticks: u64) -> u64 {
    ticks * *NANOS_PER_TICK
}

#[inline]
fn ns2t(nanos: u64) -> u64 {
    nanos / *NANOS_PER_TICK
}

pub(super) fn init_percpu() {
    tcfg::set_init_val(0);
    tcfg::set_periodic(false);
    tcfg::set_en(true);
    khal::irq::enable(TIMER_IRQ, true);
}
#[cfg(feature = "rtc")]
fn init_rtc() {
    use chrono::{TimeZone, Timelike, Utc};
    use khal::mem::PhysAddr;
    const SYS_TOY_READ0: usize = 0x2C;
    const SYS_TOY_READ1: usize = 0x30;
    const SYS_RTCCTRL: usize = 0x40;
    const TOY_ENABLE: u32 = 1 << 11;
    const OSC_ENABLE: u32 = 1 << 8;
    const LS7A_RTC_SIZE: usize = 0x1000;
    let rtc_base =
        memspace::iomap_device(PhysAddr::from_usize(RTC_PADDR), LS7A_RTC_SIZE, "ls7a-rtc")
            .unwrap_or_else(|err| panic!("failed to iomap ls7a rtc: {err:?}"));
    let rtc_base_ptr = rtc_base.as_mut_ptr();
    fn extract_bits(value: u32, range: core::ops::Range<u32>) -> u32 {
        (value >> range.start) & ((1 << (range.end - range.start)) - 1)
    }
    // SAFETY: `rtc_base_ptr` comes from a successful `iomap_device` of the
    // LS7A RTC register window, and `SYS_RTCCTRL` is a 32-bit register offset
    // within that mapped range.
    unsafe {
        (rtc_base_ptr.add(SYS_RTCCTRL) as *mut u32).write_volatile(TOY_ENABLE | OSC_ENABLE);
    }
    // SAFETY: both offsets name 32-bit TOY read registers inside the same
    // mapped LS7A RTC window described above.
    let toy_high = unsafe { (rtc_base_ptr.add(SYS_TOY_READ1) as *const u32).read_volatile() };
    // SAFETY: `SYS_TOY_READ0` is another 32-bit register in the mapped RTC
    // window, so the volatile read stays within the device MMIO region.
    let toy_low = unsafe { (rtc_base_ptr.add(SYS_TOY_READ0) as *const u32).read_volatile() };
    let date_time = Utc
        .with_ymd_and_hms(
            1900 + toy_high as i32,
            extract_bits(toy_low, 26..32),
            extract_bits(toy_low, 21..26),
            extract_bits(toy_low, 16..21),
            extract_bits(toy_low, 10..16),
            extract_bits(toy_low, 4..10),
        )
        .unwrap()
        .with_nanosecond(extract_bits(toy_low, 0..4) * NANOS_PER_MILLIS as u32)
        .unwrap();
    let Some(epoch_time_nanos) = date_time.timestamp_nanos_opt() else {
        warn!("RTC date is outside the supported nanosecond range");
        return;
    };
    let Ok(epoch_time_nanos) = u64::try_from(epoch_time_nanos) else {
        warn!("RTC date precedes the Unix epoch");
        return;
    };
    let Some(offset_ns) = epoch_time_nanos.checked_sub(t2ns(now_ticks())) else {
        warn!("RTC date precedes the monotonic boot time");
        return;
    };
    rtc_driver::init_offset_ns(offset_ns);
}
pub(super) fn early_init() {
    NANOS_PER_TICK.init_once(NANOS_PER_SEC / loongArch64::time::get_timer_freq() as u64);
    #[cfg(feature = "rtc")]
    init_rtc();
}
#[kplat::impl_dev_interface]
impl MonotonicTimerIf {
    fn now_ticks() -> u64 {
        now_ticks()
    }

    fn t2ns(ticks: u64) -> u64 {
        t2ns(ticks)
    }

    fn ns2t(nanos: u64) -> u64 {
        ns2t(nanos)
    }

    fn freq() -> u64 {
        loongArch64::time::get_timer_freq() as u64
    }

    fn interrupt_id() -> usize {
        TIMER_IRQ
    }

    fn arm_timer(deadline_ns: u64) {
        let ticks_now = now_ticks();
        let ticks_deadline = ns2t(deadline_ns);
        let init_value = ticks_deadline
            .saturating_sub(ticks_now)
            .max(MIN_TIMER_TICKS)
            .saturating_add(MIN_TIMER_TICKS - 1)
            / MIN_TIMER_TICKS
            * MIN_TIMER_TICKS;
        tcfg::set_init_val(init_value as _);
        tcfg::set_en(true);
    }

    fn handle_idle_return(_previous_ticks: u64) -> bool {
        false
    }
}

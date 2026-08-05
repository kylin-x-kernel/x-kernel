// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kbuild_config::RTC_PADDR;
use khal::time::{MonotonicTimerIf, TimerTicks};
use ktime_types::{MonotonicInstant, NANOS_PER_MILLIS, NANOS_PER_SEC, SystemTime};
use lazyinit::LazyInit;
use loongArch64::{register::tcfg, time::Time};

const TIMER_IRQ: usize = 11;
const MIN_TIMER_TICKS: u64 = 4;

static NANOS_PER_TICK: LazyInit<u64> = LazyInit::new();

#[inline]
fn read_timer_ticks_raw() -> u64 {
    Time::read() as _
}

#[inline]
fn ticks_to_nanos(ticks: u64) -> u64 {
    ticks * *NANOS_PER_TICK
}

#[inline]
fn nanos_to_ticks(nanos: u64) -> u64 {
    nanos / *NANOS_PER_TICK
}

pub(super) fn init_percpu() {
    tcfg::set_init_val(0);
    tcfg::set_periodic(false);
    tcfg::set_en(true);
    khal::irq::enable(TIMER_IRQ, true);
}
#[cfg(feature = "rtc")]
fn read_rtc() -> SystemTime {
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
    SystemTime::from_unix_parts(date_time.timestamp(), date_time.nanosecond())
        .expect("chrono returns normalized nanoseconds")
}
pub(super) fn early_init() {
    NANOS_PER_TICK.init_once(NANOS_PER_SEC / loongArch64::time::get_timer_freq() as u64);
    #[cfg(feature = "rtc")]
    ktime::initialize_realtime(read_rtc());
}
#[kplat::impl_dev_interface]
impl MonotonicTimerIf {
    fn now_ticks() -> TimerTicks {
        TimerTicks::from_raw(read_timer_ticks_raw())
    }

    fn ticks_to_span(ticks: TimerTicks) -> ktime_types::TimeSpan {
        ktime_types::TimeSpan::from_nanos(ticks_to_nanos(ticks.as_raw()))
    }

    fn span_to_ticks(span: ktime_types::TimeSpan) -> TimerTicks {
        TimerTicks::from_raw(nanos_to_ticks(span.as_nanos_u64_saturating()))
    }

    fn freq() -> u64 {
        loongArch64::time::get_timer_freq() as u64
    }

    fn interrupt_id() -> usize {
        TIMER_IRQ
    }

    fn arm_timer(deadline: MonotonicInstant) {
        let ticks_now = read_timer_ticks_raw();
        let deadline_ns = deadline.as_nanos_u64_saturating();
        let ticks_deadline = nanos_to_ticks(deadline_ns);
        let init_value = ticks_deadline
            .saturating_sub(ticks_now)
            .max(MIN_TIMER_TICKS)
            .saturating_add(MIN_TIMER_TICKS - 1)
            / MIN_TIMER_TICKS
            * MIN_TIMER_TICKS;
        tcfg::set_init_val(init_value as _);
        tcfg::set_en(true);
    }

    fn handle_idle_return(_previous_ticks: TimerTicks) -> bool {
        false
    }
}

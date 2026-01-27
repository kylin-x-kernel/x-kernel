use core::time::Duration;

use kplat_macros::device_interface;

pub type ClockTime = Duration;

pub const MS_SEC: u64 = 1_000;
pub const US_SEC: u64 = 1_000_000;
pub const NS_SEC: u64 = 1_000_000_000;
pub const NS_MS: u64 = 1_000_000;
pub const NS_US: u64 = 1_000;

#[device_interface]
pub trait GlobalTimer {
    fn now_ticks() -> u64;
    fn t2ns(t: u64) -> u64;
    fn freq() -> u64; // Hz
    fn ns2t(ns: u64) -> u64;
    fn offset_ns() -> u64;

    #[cfg(feature = "irq")]
    fn interrupt_id() -> usize;

    #[cfg(feature = "irq")]
    fn arm_timer(deadline: u64);
}

pub fn now_ns() -> u64 {
    t2ns(now_ticks())
}

pub fn now() -> ClockTime {
    ClockTime::from_nanos(now_ns())
}

pub fn wall_ns() -> u64 {
    now_ns() + offset_ns()
}

pub fn wall() -> ClockTime {
    ClockTime::from_nanos(wall_ns())
}

pub fn spin_wait(d: Duration) {
    spin_until(wall() + d);
}

pub fn spin_until(dl: ClockTime) {
    while wall() < dl {
        core::hint::spin_loop();
    }
}

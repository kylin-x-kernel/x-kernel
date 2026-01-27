//! Time-related operations.

pub use core::time::Duration;
pub type TimeValue = Duration;

// Aliases for kplat names if needed locally or exposed
pub use kplat::timer::{
    MS_SEC, NS_MS, NS_SEC, NS_SEC as NANOS_PER_SEC, NS_US, NS_US as NANOS_PER_MICROS, US_SEC, freq,
    now, now as monotonic_time, now_ns as monotonic_time_nanos, now_ns, now_ticks, ns2t, offset_ns,
    spin_until, spin_wait, t2ns, wall as wall_time, wall, wall_ns as wall_time_nanos, wall_ns,
};
#[cfg(feature = "irq")]
pub use kplat::timer::{arm_timer, interrupt_id};

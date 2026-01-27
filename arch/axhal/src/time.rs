//! Time-related operations.

pub use core::time::Duration;
pub type TimeValue = Duration;

pub use kplat::timer::{
    US_SEC, MS_SEC, NS_US, NS_MS, NS_SEC,
    spin_wait, spin_until, now_ticks, offset_ns,
    now as monotonic_time, now_ns as monotonic_time_nanos,
    ns2t, t2ns, freq,
    wall as wall_time, wall_ns as wall_time_nanos,
};

// Aliases for kplat names if needed locally or exposed
pub use kplat::timer::now;
pub use kplat::timer::now_ns;
pub use kplat::timer::wall;
pub use kplat::timer::wall_ns;

#[cfg(feature = "irq")]
pub use kplat::timer::{interrupt_id, arm_timer};
pub use kplat::timer::NS_US as NANOS_PER_MICROS;
pub use kplat::timer::NS_SEC as NANOS_PER_SEC;

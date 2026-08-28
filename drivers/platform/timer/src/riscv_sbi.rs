// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use riscv::register::time;

use crate::TimerSource;

static TIMER_IRQ: AtomicUsize = AtomicUsize::new(0);
static TIMER_FREQ_HZ: AtomicU64 = AtomicU64::new(0);
static NANOS_PER_TICK: AtomicU64 = AtomicU64::new(0);

#[kplat::impl_dev_interface]
impl khal::time::MonotonicTimerIf {
    fn now_ticks() -> khal::time::TimerTicks {
        now_ticks()
    }

    fn ticks_to_span(ticks: khal::time::TimerTicks) -> ktime_types::TimeSpan {
        ktime_types::TimeSpan::from_nanos(ticks_to_nanos(ticks.as_raw()))
    }

    fn freq() -> u64 {
        freq()
    }

    fn span_to_ticks(span: ktime_types::TimeSpan) -> khal::time::TimerTicks {
        khal::time::TimerTicks::from_raw(nanos_to_ticks(span.as_nanos_u64_saturating()))
    }

    fn interrupt_id() -> usize {
        interrupt_id()
    }

    fn arm_timer(deadline: ktime_types::MonotonicInstant) {
        arm_timer(deadline)
    }

    fn disarm_timer() {
        disarm_timer()
    }

    fn handle_idle_return(_previous_ticks: khal::time::TimerTicks) -> bool {
        false
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimerConfig {
    pub irq: usize,
    pub frequency_hz: u64,
    pub source: TimerSource,
}

impl TimerConfig {
    pub const fn platform_static(irq: usize, frequency_hz: u64) -> Self {
        Self {
            irq,
            frequency_hz,
            source: TimerSource::PlatformStatic,
        }
    }
}

pub fn init(config: TimerConfig) {
    assert!(config.irq != 0, "RISC-V SBI timer IRQ must be non-zero");
    assert!(
        config.frequency_hz != 0,
        "RISC-V SBI timer frequency must be non-zero"
    );
    TIMER_IRQ.store(config.irq, Ordering::Relaxed);
    TIMER_FREQ_HZ.store(config.frequency_hz, Ordering::Relaxed);
    NANOS_PER_TICK.store(
        ktime_types::NANOS_PER_SEC / config.frequency_hz,
        Ordering::Relaxed,
    );
}

pub fn init_percpu() {
    sbi_rt::set_timer(0);
}

#[inline]
pub fn now_ticks() -> khal::time::TimerTicks {
    khal::time::TimerTicks::from_raw(read_timer_ticks_raw())
}

#[inline]
fn read_timer_ticks_raw() -> u64 {
    time::read() as u64
}

#[inline]
pub(crate) fn ticks_to_nanos(ticks: u64) -> u64 {
    ticks * NANOS_PER_TICK.load(Ordering::Relaxed)
}

#[inline]
pub(crate) fn nanos_to_ticks(nanos: u64) -> u64 {
    nanos / NANOS_PER_TICK.load(Ordering::Relaxed)
}

#[inline]
pub fn freq() -> u64 {
    TIMER_FREQ_HZ.load(Ordering::Relaxed)
}

#[inline]
pub fn interrupt_id() -> usize {
    let irq = TIMER_IRQ.load(Ordering::Relaxed);
    assert!(irq != 0, "RISC-V SBI timer not initialized");
    irq
}

pub fn arm_timer(deadline: ktime_types::MonotonicInstant) {
    let deadline_ns = deadline.as_nanos_u64_saturating();
    sbi_rt::set_timer(nanos_to_ticks(deadline_ns));
}

pub fn disarm_timer() {
    // A compare value far in the future disables practical timer IRQs until re-armed.
    sbi_rt::set_timer(u64::MAX);
}

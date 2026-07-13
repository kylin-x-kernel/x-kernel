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
    fn now_ticks() -> u64 {
        now_ticks()
    }

    fn t2ns(ticks: u64) -> u64 {
        t2ns(ticks)
    }

    fn freq() -> u64 {
        freq()
    }

    fn ns2t(nanos: u64) -> u64 {
        ns2t(nanos)
    }

    fn interrupt_id() -> usize {
        interrupt_id()
    }

    fn arm_timer(deadline_ns: u64) {
        arm_timer(deadline_ns)
    }

    fn handle_idle_return(_previous_ticks: u64) -> bool {
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
    NANOS_PER_TICK.store(1_000_000_000 / config.frequency_hz, Ordering::Relaxed);
}

pub fn init_percpu() {
    sbi_rt::set_timer(0);
}

#[inline]
pub fn now_ticks() -> u64 {
    time::read() as u64
}

#[inline]
pub fn t2ns(ticks: u64) -> u64 {
    ticks * NANOS_PER_TICK.load(Ordering::Relaxed)
}

#[inline]
pub fn ns2t(nanos: u64) -> u64 {
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

pub fn arm_timer(deadline_ns: u64) {
    sbi_rt::set_timer(ns2t(deadline_ns));
}

#[inline]
pub fn rtc_now_nanos() -> u64 {
    t2ns(now_ticks())
}

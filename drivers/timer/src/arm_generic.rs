// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{
    convert::TryInto,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
};

use aarch64_cpu::registers::{
    CNTFRQ_EL0, CNTP_CTL_EL0, CNTP_TVAL_EL0, CNTPCT_EL0, Readable, Writeable,
};
use int_ratio::Ratio;

use crate::TimerSource;

static TIMER_IRQ: AtomicUsize = AtomicUsize::new(0);
static TIMER_FREQ_HZ: AtomicU64 = AtomicU64::new(0);
static mut CNTPCT_TO_NANOS_RATIO: Ratio = Ratio::zero();
static mut NANOS_TO_CNTPCT_RATIO: Ratio = Ratio::zero();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimerConfig {
    pub irq: usize,
    pub frequency_hz: Option<u64>,
    pub source: TimerSource,
}

impl TimerConfig {
    pub const fn platform_static(irq: usize) -> Self {
        Self {
            irq,
            frequency_hz: None,
            source: TimerSource::PlatformStatic,
        }
    }
}

struct ArmGenericMonotonicTimer;

#[kplat::impl_dev_interface]
impl khal::time::MonotonicTimerIf for ArmGenericMonotonicTimer {
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
}

pub fn init(config: TimerConfig) {
    assert!(config.irq != 0, "ARM generic timer IRQ must be non-zero");
    TIMER_IRQ.store(config.irq, Ordering::Relaxed);

    let freq = config.frequency_hz.unwrap_or_else(|| CNTFRQ_EL0.get());
    assert!(freq != 0, "ARM generic timer frequency must be non-zero");
    assert!(
        u32::try_from(freq).is_ok(),
        "ARM generic timer frequency must fit in u32"
    );
    TIMER_FREQ_HZ.store(freq, Ordering::Relaxed);
    unsafe {
        CNTPCT_TO_NANOS_RATIO = Ratio::new(1_000_000_000u32, freq as u32);
        NANOS_TO_CNTPCT_RATIO = CNTPCT_TO_NANOS_RATIO.inverse();
    }
}

pub fn init_percpu() {
    let irq = interrupt_id();
    CNTP_CTL_EL0.write(CNTP_CTL_EL0::ENABLE::SET);
    CNTP_TVAL_EL0.set(0);
    khal::irq::enable(irq, true);
}

#[inline]
pub fn now_ticks() -> u64 {
    CNTPCT_EL0.get()
}

#[inline]
pub fn t2ns(ticks: u64) -> u64 {
    unsafe { CNTPCT_TO_NANOS_RATIO.mul_trunc(ticks) }
}

#[inline]
pub fn ns2t(nanos: u64) -> u64 {
    unsafe { NANOS_TO_CNTPCT_RATIO.mul_trunc(nanos) }
}

#[inline]
pub fn freq() -> u64 {
    TIMER_FREQ_HZ.load(Ordering::Relaxed)
}

#[inline]
pub fn interrupt_id() -> usize {
    let irq = TIMER_IRQ.load(Ordering::Relaxed);
    assert!(irq != 0, "ARM generic timer not initialized");
    irq
}

pub fn arm_timer(deadline_ns: u64) {
    let cnptct = CNTPCT_EL0.get();
    let cnptct_deadline = ns2t(deadline_ns);
    if cnptct < cnptct_deadline {
        let interval = cnptct_deadline - cnptct;
        debug_assert!(interval <= u32::MAX as u64);
        CNTP_TVAL_EL0.set(interval);
    } else {
        CNTP_TVAL_EL0.set(0);
    }
}

pub fn config_from_device_tree() -> Option<TimerConfig> {
    let node = of::find_compatible("arm,armv8-timer")
        .or_else(|| of::find_compatible("arm,armv7-timer"))?;
    Some(TimerConfig {
        irq: timer_irq_from_device_tree(node)?,
        frequency_hz: timer_frequency_from_device_tree(node),
        source: TimerSource::DeviceTree,
    })
}

fn timer_irq_from_device_tree(node: of::FdtNode<'static, 'static>) -> Option<usize> {
    let interrupts = node.property("interrupts")?.value;
    let names = node.property("interrupt-names").map(|prop| prop.value);

    preferred_timer_interrupt_index(names)
        .and_then(|index| interrupt_spec_at(interrupts, index))
        .or_else(|| interrupt_spec_at(interrupts, 1))
        .or_else(|| interrupt_spec_at(interrupts, 0))
}

fn preferred_timer_interrupt_index(names: Option<&[u8]>) -> Option<usize> {
    let names = names?;
    for preferred in ["phys", "virt", "sec-phys", "hyp-phys", "hyp-virt"] {
        if let Some(index) = interrupt_names(names).position(|name| name == preferred) {
            return Some(index);
        }
    }
    None
}

fn interrupt_names(names: &[u8]) -> impl Iterator<Item = &str> {
    names.split(|byte| *byte == 0).filter_map(|name| {
        if name.is_empty() {
            None
        } else {
            core::str::from_utf8(name).ok()
        }
    })
}

fn interrupt_spec_at(specs: &[u8], index: usize) -> Option<usize> {
    let spec = specs.chunks_exact(12).nth(index)?;
    let irq_type = read_be_u32(spec, 0)?;
    let irq_num = read_be_u32(spec, 1)?;
    let intid = match irq_type {
        0 => 32 + irq_num,
        1 => 16 + irq_num,
        _ => return None,
    };
    Some(intid as usize)
}

fn timer_frequency_from_device_tree(node: of::FdtNode<'static, 'static>) -> Option<u64> {
    let value = node.property("clock-frequency")?.value;
    match value.len() {
        4 => Some(u32::from_be_bytes(value.try_into().ok()?) as u64),
        8 => Some(u64::from_be_bytes(value.try_into().ok()?)),
        _ => None,
    }
}

fn read_be_u32(spec: &[u8], index: usize) -> Option<u32> {
    let start = index.checked_mul(4)?;
    let bytes: [u8; 4] = spec.get(start..start + 4)?.try_into().ok()?;
    Some(u32::from_be_bytes(bytes))
}

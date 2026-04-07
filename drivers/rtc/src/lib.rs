// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_std]

use core::sync::atomic::{AtomicU64, Ordering};

use kplat::memory::{PhysAddr, VirtAddr};

#[cfg(all(feature = "cmos", target_arch = "x86_64"))]
pub mod cmos;
#[cfg(feature = "goldfish")]
pub mod goldfish;
#[cfg(feature = "pl031")]
pub mod pl031;

static RTC_EPOCHOFFSET_NANOS: AtomicU64 = AtomicU64::new(0);

struct DriverRtcIfImpl;

#[kplat::impl_dev_interface]
impl khal::rtc::RtcIf for DriverRtcIfImpl {
    fn offset_ns() -> u64 {
        offset_ns()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtcKind {
    Goldfish,
    Pl031,
    Cmos,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtcTransport {
    Mmio { paddr: PhysAddr, size: usize },
    MmioMapped { vaddr: VirtAddr },
    Platform,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtcSource {
    DeviceTree,
    Acpi,
    PlatformStatic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtcConfig {
    pub kind: RtcKind,
    pub transport: RtcTransport,
    pub source: RtcSource,
}

impl RtcConfig {
    pub const fn mmio(kind: RtcKind, paddr: PhysAddr, size: usize, source: RtcSource) -> Self {
        Self {
            kind,
            transport: RtcTransport::Mmio { paddr, size },
            source,
        }
    }

    pub const fn platform(kind: RtcKind, source: RtcSource) -> Self {
        Self {
            kind,
            transport: RtcTransport::Platform,
            source,
        }
    }

    pub const fn mmio_mapped(kind: RtcKind, vaddr: VirtAddr, source: RtcSource) -> Self {
        Self {
            kind,
            transport: RtcTransport::MmioMapped { vaddr },
            source,
        }
    }
}

#[inline]
pub fn offset_ns() -> u64 {
    RTC_EPOCHOFFSET_NANOS.load(Ordering::Relaxed)
}

#[inline]
pub fn init_offset_ns(offset: u64) {
    RTC_EPOCHOFFSET_NANOS.store(offset, Ordering::Relaxed);
}

#[inline]
pub fn init_unix_timestamp_offset(unix_seconds: u64, now_nanos: u64) {
    let epoch_time_nanos = unix_seconds.saturating_mul(1_000_000_000);
    init_offset_ns(epoch_time_nanos.saturating_sub(now_nanos));
}

pub fn config_from_device_tree() -> Option<RtcConfig> {
    #[cfg(feature = "pl031")]
    if let Some(config) =
        mmio_config_from_device_tree("arm,pl031", RtcKind::Pl031, RtcSource::DeviceTree)
    {
        return Some(config);
    }

    #[cfg(feature = "goldfish")]
    if let Some(config) = mmio_config_from_device_tree(
        "google,goldfish-rtc",
        RtcKind::Goldfish,
        RtcSource::DeviceTree,
    ) {
        return Some(config);
    }

    None
}

pub fn init(config: RtcConfig, _now_nanos: u64) {
    match (config.kind, config.transport) {
        #[cfg(feature = "goldfish")]
        (RtcKind::Goldfish, RtcTransport::MmioMapped { vaddr }) => {
            goldfish::init_mapped(vaddr, _now_nanos);
        }
        #[cfg(feature = "pl031")]
        (RtcKind::Pl031, RtcTransport::MmioMapped { vaddr }) => {
            pl031::init_mapped(vaddr, _now_nanos);
        }
        #[cfg(all(feature = "cmos", target_arch = "x86_64"))]
        (RtcKind::Cmos, RtcTransport::Platform) => {
            cmos::init_platform(_now_nanos);
        }
        (kind, transport) => {
            panic!("unsupported rtc configuration: kind={kind:?} transport={transport:?}");
        }
    }
}

#[cfg(any(feature = "pl031", feature = "goldfish"))]
fn mmio_config_from_device_tree(
    compatible: &str,
    kind: RtcKind,
    source: RtcSource,
) -> Option<RtcConfig> {
    let node = of::find_compatible(compatible)?;
    let reg = node.reg()?.next()?;
    Some(RtcConfig::mmio(
        kind,
        PhysAddr::from_usize(reg.starting_address as usize),
        reg.size,
        source,
    ))
}

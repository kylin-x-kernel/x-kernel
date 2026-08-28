// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Persistent real-time clock discovery and sampling.

#![no_std]
#![warn(missing_docs)]

use khal::mem::{PhysAddr, VirtAddr};
use ktime_types::SystemTime;

#[cfg(all(feature = "cmos", target_arch = "x86_64"))]
mod cmos;
#[cfg(feature = "goldfish")]
mod goldfish;
#[cfg(feature = "pl031")]
mod pl031;

fn system_time_from_unsigned_seconds(seconds: u64) -> Option<SystemTime> {
    i64::try_from(seconds)
        .ok()
        .map(SystemTime::from_unix_seconds)
}

/// Supported persistent RTC device kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtcKind {
    /// Goldfish virtual RTC.
    Goldfish,
    /// ARM PrimeCell PL031 RTC.
    Pl031,
    /// PC-compatible CMOS RTC.
    Cmos,
}

/// Transport used to access an RTC device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtcTransport {
    /// An unmapped MMIO register range.
    Mmio {
        /// Physical base address of the register range.
        paddr: PhysAddr,
        /// Size of the register range in bytes.
        size: usize,
    },
    /// A mapped MMIO register range.
    MmioMapped {
        /// Virtual base address of the mapped register range.
        vaddr: VirtAddr,
    },
    /// A platform-defined transport such as x86 I/O ports.
    Platform,
}

/// Firmware or platform source of an RTC description.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtcSource {
    /// Device-tree discovery.
    DeviceTree,
    /// ACPI discovery.
    Acpi,
    /// Static platform configuration.
    PlatformStatic,
}

/// RTC device configuration and transport description.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtcConfig {
    /// RTC device kind.
    pub kind: RtcKind,
    /// RTC access transport.
    pub transport: RtcTransport,
    /// Source of this configuration.
    pub source: RtcSource,
}

impl RtcConfig {
    /// Creates an unmapped MMIO RTC configuration.
    pub const fn mmio(kind: RtcKind, paddr: PhysAddr, size: usize, source: RtcSource) -> Self {
        Self {
            kind,
            transport: RtcTransport::Mmio { paddr, size },
            source,
        }
    }

    /// Creates a platform-transport RTC configuration.
    pub const fn platform(kind: RtcKind, source: RtcSource) -> Self {
        Self {
            kind,
            transport: RtcTransport::Platform,
            source,
        }
    }

    /// Creates an already-mapped MMIO RTC configuration.
    pub const fn mmio_mapped(kind: RtcKind, vaddr: VirtAddr, source: RtcSource) -> Self {
        Self {
            kind,
            transport: RtcTransport::MmioMapped { vaddr },
            source,
        }
    }
}

/// Discovers an RTC configuration from the device tree.
#[cfg(any(feature = "pl031", feature = "goldfish"))]
fn config_from_device_tree() -> Option<RtcConfig> {
    #[cfg(feature = "pl031")]
    if let Some(config) = pl031_config_from_device_tree() {
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

#[cfg(feature = "pl031")]
fn pl031_config_from_device_tree() -> Option<RtcConfig> {
    if let Some(config) =
        mmio_config_from_device_tree("arm,pl031", RtcKind::Pl031, RtcSource::DeviceTree)
    {
        return Some(config);
    }

    const PL031_PRIMECELL_PERIPHID: u32 = 0x41030;
    let node = of::fdt()?.all_nodes().find(|node| {
        node.compatibles()
            .any(|compatible| compatible == "arm,primecell")
            && of::property_u32(*node, "arm,primecell-periphid") == Some(PL031_PRIMECELL_PERIPHID)
    })?;
    let reg = node.reg()?.next()?;
    Some(RtcConfig::mmio(
        RtcKind::Pl031,
        PhysAddr::from_usize(reg.starting_address as usize),
        reg.size,
        RtcSource::DeviceTree,
    ))
}

/// Discovers, maps, and samples the RTC described by the device tree.
///
/// Returns `None` when no supported RTC is present or the device does not
/// provide a usable sample.
///
/// # Panics
///
/// Panics if the discovered register range cannot be mapped or its RTC
/// configuration is unsupported.
#[cfg(any(feature = "pl031", feature = "goldfish"))]
pub fn read_from_device_tree() -> Option<SystemTime> {
    let config = config_from_device_tree()?;
    let mapped = match config {
        RtcConfig {
            kind,
            transport: RtcTransport::Mmio { paddr, size },
            source,
        } => {
            let name = match kind {
                RtcKind::Goldfish => "goldfish-rtc",
                RtcKind::Pl031 => "pl031",
                RtcKind::Cmos => "rtc",
            };
            RtcConfig::mmio_mapped(
                kind,
                memspace::iomap_device(paddr, size, name)
                    .unwrap_or_else(|err| panic!("failed to iomap {name}: {err:?}")),
                source,
            )
        }
        other => panic!("unsupported rtc configuration: {other:?}"),
    };
    read(mapped)
}

/// Samples an RTC using an initialized configuration.
///
/// Returns `None` when a mapped configuration has no usable address.
///
/// # Panics
///
/// Panics if support for the configured RTC kind or transport is not enabled.
pub fn read(config: RtcConfig) -> Option<SystemTime> {
    match (config.kind, config.transport) {
        #[cfg(feature = "goldfish")]
        (RtcKind::Goldfish, RtcTransport::MmioMapped { vaddr }) => goldfish::read_mapped(vaddr),
        #[cfg(feature = "pl031")]
        (RtcKind::Pl031, RtcTransport::MmioMapped { vaddr }) => pl031::read_mapped(vaddr),
        #[cfg(all(feature = "cmos", target_arch = "x86_64"))]
        (RtcKind::Cmos, RtcTransport::Platform) => cmos::read_platform(),
        (kind, transport) => {
            panic!("unsupported rtc configuration: kind={kind:?} transport={transport:?}");
        }
    }
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert_eq, def_test};

    use super::*;

    #[def_test]
    fn unsigned_seconds_reject_values_outside_system_time() {
        assert_eq!(
            system_time_from_unsigned_seconds(i64::MAX as u64),
            Some(SystemTime::from_unix_seconds(i64::MAX))
        );
        assert_eq!(system_time_from_unsigned_seconds(u64::MAX), None);
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

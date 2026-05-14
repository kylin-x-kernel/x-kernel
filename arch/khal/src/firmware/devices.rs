// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Firmware-described **device** abstraction.
//!
//! This module is the single seam between firmware-table parsers (DT / ACPI)
//! and consumers that need to know what hardware exists and how to reach it
//! (e.g. `drivers/kdriver` bus backends, `drivers/pci` host detection,
//! `drivers/virtio` interrupt routing).
//!
//! Layering rationale
//! ------------------
//! The `of` and `acpi` crates are *raw firmware-table readers*. Higher layers
//! should not couple to a particular firmware flavor. Instead they call the
//! flavor-neutral entries here, which try Device Tree first and fall back to
//! ACPI where applicable. New firmware sources (UEFI device-paths, SMBIOS,
//! platform-static tables) plug in by extending the helpers in this file
//! without touching the consumers.
//!
//! Re-exports
//! ----------
//! Common interrupt / PCI host description types live in `of` today and are
//! re-exported here so callers depend only on `khal::firmware::devices::*`.

#[cfg(target_arch = "x86_64")]
use lazyinit::LazyInit;
pub use of::{
    InterruptControllerKind, InterruptInfo, InterruptTrigger, PciHostCam, PciHostInfo, PciRangeInfo,
};

/// Cached IO-APIC physical address from ACPI MADT.
#[cfg(target_arch = "x86_64")]
static IO_APIC_CACHE: LazyInit<Option<usize>> = LazyInit::new();

/// Which firmware table the device description was read from.
///
/// Kept as a flat enum here (rather than borrowing `kdriver`'s
/// `FirmwareOrigin`) so this layer stays free of driver-framework deps.
/// Callers that need to record the origin can map this 1:1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirmwareSource {
    /// Flattened Device Tree.
    DeviceTree,
    /// ACPI tables.
    Acpi,
}

/// One device described by firmware, normalized for the kernel driver
/// framework.
///
/// `firmware_id` is the matched raw firmware identity string (a DT
/// `compatible` today; ACPI `_HID/_CID` in the future). Consumers keep this
/// raw identifier and let the driver registry perform Linux-style matching
/// against per-driver firmware ID tables. Resources are optional because not
/// every firmware node carries them. `source` records which firmware table the
/// description came from so consumers do not have to hard-code an origin.
#[derive(Debug, Clone, Copy)]
pub struct FirmwareDevice {
    pub firmware_id: &'static str,
    pub source: FirmwareSource,
    pub mmio: Option<MmioResource>,
    pub irq: Option<InterruptInfo>,
}

#[derive(Debug, Clone, Copy)]
pub struct MmioResource {
    pub base: usize,
    pub size: usize,
}

/// Returns `true` if any firmware device description (DT or ACPI) has been
/// initialized.
///
/// Used by platform-static fallback backends to decide whether to register
/// hard-coded peripherals (which would otherwise duplicate firmware-described
/// nodes).
pub fn has_device_description() -> bool {
    if of::fdt().is_some() {
        return true;
    }
    #[cfg(target_arch = "x86_64")]
    if acpi::desc().is_some() {
        return true;
    }
    false
}

/// Locate the primary PCI host bridge.
///
/// Tries Device Tree (`pci-host-(e)cam-generic`) first, then ACPI MCFG. Only
/// segment 0 is considered. Callers that need caching should cache the result
/// themselves.
pub fn pci_host() -> Option<PciHostInfo> {
    if let Some(host) = of::generic_pci_host_info() {
        return Some(host);
    }

    #[cfg(target_arch = "x86_64")]
    {
        if let Some(mcfg) = acpi::find_mcfg_from_init()
            && mcfg.pci_segment == 0
        {
            return Some(PciHostInfo {
                cam: PciHostCam::Ecam,
                ecam_base: mcfg.base_address,
                ecam_size: ((mcfg.end_bus as u64).saturating_sub(mcfg.start_bus as u64) + 1) << 20,
                bus_start: mcfg.start_bus,
                bus_end: mcfg.end_bus,
            });
        }
    }

    None
}

/// Returns the firmware source that described the primary PCI host bridge,
/// or `None` if no firmware-described host was found.
///
/// Mirrors the discovery priority used by [`pci_host`]: Device Tree first,
/// then ACPI MCFG. Useful for callers that need to record provenance (e.g.
/// the device-database `FirmwareOrigin`) without re-doing the table walk.
pub fn pci_host_source() -> Option<FirmwareSource> {
    if of::generic_pci_host_info().is_some() {
        return Some(FirmwareSource::DeviceTree);
    }

    #[cfg(target_arch = "x86_64")]
    {
        if let Some(mcfg) = acpi::find_mcfg_from_init()
            && mcfg.pci_segment == 0
        {
            return Some(FirmwareSource::Acpi);
        }
    }

    None
}

/// Returns the first non-prefetchable PCI memory range from firmware.
///
/// Tries Device Tree first; on x86 falls back to a best-effort byte scan of
/// the ACPI DSDT `_CRS` resource template (see
/// [`acpi::find_pci_host_mem_window_from_init`]). Callers that need caching
/// should cache the result themselves.
pub fn pci_bar_range() -> Option<PciRangeInfo> {
    if let Some(range) = of::generic_pci_non_prefetchable_mem_range() {
        return Some(range);
    }

    #[cfg(target_arch = "x86_64")]
    {
        if let Some(window) = acpi::find_pci_host_mem_window_from_init() {
            return Some(PciRangeInfo {
                cpu_base: window.base,
                size: window.size,
                prefetchable: window.prefetchable,
            });
        }
    }

    None
}

/// Resolve a legacy INTx route for a PCI BDF/pin via firmware.
///
/// DT-only. ACPI legacy routing comes from `_PRT`; not yet parsed.
pub fn pci_legacy_irq(bus: u8, device: u8, function: u8, pin: u8) -> Option<InterruptInfo> {
    of::generic_pci_legacy_interrupt(bus, device, function, pin)
}

/// x86: physical address of the (first) IO-APIC reported by ACPI MADT.
///
/// Cached on first call so the MADT walk runs at most once per boot.
#[cfg(target_arch = "x86_64")]
pub fn io_apic_paddr() -> Option<usize> {
    IO_APIC_CACHE.call_once(|| acpi::find_apic_from_init().and_then(|info| info.io_apic_address));
    *IO_APIC_CACHE.get().expect("IO-APIC cache not initialized")
}

/// Iterate firmware-described devices, mapping each node to a driver-visible
/// firmware identity via the caller-provided `matcher`.
///
/// `matcher` is invoked once per *firmware compatible string* on each node;
/// returning `Some(firmware_id)` selects the node and stops checking that
/// node's remaining compatibles. The visitor then receives the matched raw
/// firmware ID plus the node's primary MMIO / IRQ resources, if present.
///
/// Today this walks the device tree. ACPI `_HID/_CID` walks can be added here
/// without touching callers — the matcher signature stays string-based and
/// firmware-neutral.
pub fn for_each_compatible(
    mut matcher: impl FnMut(&str) -> Option<&'static str>,
    mut visitor: impl FnMut(FirmwareDevice),
) {
    let Some(fdt) = of::fdt() else {
        return;
    };

    for node in fdt.all_nodes() {
        let Some(firmware_id) = node.compatibles().find_map(&mut matcher) else {
            continue;
        };

        let mmio = node
            .reg()
            .and_then(|mut regs| regs.next())
            .map(|reg| MmioResource {
                base: reg.starting_address as usize,
                size: reg.size,
            });
        let irq = of::first_interrupt_desc(node);

        visitor(FirmwareDevice {
            firmware_id,
            source: FirmwareSource::DeviceTree,
            mmio,
            irq,
        });
    }
}

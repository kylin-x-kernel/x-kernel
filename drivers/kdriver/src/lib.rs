// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! [x-kernel] device drivers.
//!
//! The primary entry point is [`init_drivers`], which runs discovery and
//! probes drivers against long-lived device objects. Drivers publish runtime
//! devices into typed `kclass` registries.
//!
//! Device categories live in the typed `kclass` layer.
//!
//! Initialization has three deliberately separate paths:
//!
//! - Platform `early_driver_init` brings up timer, IRQ, and boot console pieces
//!   that must work before the generic driver model can enumerate descriptors.
//! - The descriptor-first path enumerates buses, matches registered drivers,
//!   probes unpublished `DeviceObject`s, then publishes active runtime devices.
//! - The adoption path is reserved for early devices that are already running
//!   but still need a runtime device-model object, such as the boot console.
//!
//! Supports static and dynamic device models via the `dyn` feature.

#![no_std]
#![feature(associated_type_defaults)]

extern crate alloc;

#[macro_use]
extern crate log;

mod bus;
pub mod driver_registry;
mod enumeration;
mod manager;
mod resource;

pub use pci::set_pci_config_space;

pub use self::{
    bus::manager::{BusManager, default_bus_manager},
    driver_registry::{BusOwnershipSummary, DriverOwnershipSummary, OwnershipSummary},
    manager::{DeviceManager, device_manager, discover_unified},
    resource::{devm_alloc_coherent, devm_iomap, devm_request_irq, install_resource_provider},
};

#[cfg(feature = "virtio")]
fn iomap_mmio(
    paddr: usize,
    size: usize,
    name: &'static str,
) -> driver_base::DriverResult<core::ptr::NonNull<u8>> {
    let vaddr = memspace::iomap_device(paddr.into(), size, name).map_err(|err| {
        warn!(
            "failed to iomap {name} at [PA:{:#x}, PA:{:#x}): {:?}",
            paddr,
            paddr.saturating_add(size),
            err
        );
        match err {
            memspace::IoMapError::NoMemory => driver_base::DriverError::NoMemory,
            memspace::IoMapError::InvalidRange => driver_base::DriverError::InvalidInput,
            memspace::IoMapError::MappingFailed => driver_base::DriverError::Io,
        }
    })?;
    core::ptr::NonNull::new(vaddr.as_mut_ptr()).ok_or(driver_base::DriverError::BadState)
}

#[cfg(any(
    feature = "ahci",
    feature = "sdmmc",
    feature = "ixgbe",
    feature = "fxmac"
))]
fn first_mmio_resource(
    device: &kdevice::DeviceObject,
) -> driver_base::DriverResult<kdevice::MmioRegion> {
    device
        .first_mmio()
        .ok_or(driver_base::DriverError::InvalidInput)
}

#[cfg(any(feature = "ahci", feature = "sdmmc", feature = "ixgbe"))]
fn iomap_first_mmio(
    device: &kdevice::DeviceObject,
    name: &'static str,
) -> driver_base::DriverResult<(core::ptr::NonNull<u8>, usize)> {
    let mmio = first_mmio_resource(device)?;
    let ptr = resource::devm_iomap(device, mmio, name)?;
    Ok((ptr, mmio.size))
}

/// Initialize device drivers and populate typed `kclass` device classes.
pub fn init_drivers() {
    info!("Initialize device drivers...");

    // Initialize global object/metadata stores before any device is activated.
    kdevice::init_device_registry();

    let _registry = discover_unified();

    log_class_summary();
    log_device_core_summary();
    log_device_ownership_summary();
}

/// Snapshot the current long-lived device topology from the driver core.
pub fn current_device_ownership() -> kdevice::DeviceTopology {
    kdevice::device_topology()
}

/// Summarize the current long-lived ownership graph through the `kdriver` facade.
pub fn current_device_ownership_summary() -> OwnershipSummary {
    let ownership = current_device_ownership();
    OwnershipSummary {
        bus_count: ownership.bus_cores().count(),
        driver_count: ownership.driver_cores().count(),
        device_count: ownership.device_cores().count(),
    }
}

/// Summarize current device ownership per bus through the `kdriver` facade.
pub fn current_bus_ownership_summaries() -> alloc::vec::Vec<BusOwnershipSummary> {
    let ownership = current_device_ownership();
    ownership
        .buses()
        .map(|bus| BusOwnershipSummary {
            id: bus.info.id,
            name: bus.info.name,
            device_count: ownership.devices_on_bus(bus.info.id).count(),
        })
        .collect()
}

/// Summarize current device ownership per driver through the `kdriver` facade.
pub fn current_driver_ownership_summaries() -> alloc::vec::Vec<DriverOwnershipSummary> {
    let ownership = current_device_ownership();
    ownership
        .drivers()
        .map(|driver| DriverOwnershipSummary {
            id: driver.info.id,
            name: driver.info.name,
            device_kind: driver.info.device_kind,
            device_count: ownership.devices_for_driver(driver.info.id).count(),
        })
        .collect()
}

/// Enumerate current device cores attached to one bus through the `kdriver` facade.
pub fn current_devices_on_bus(bus: kdevice::BusId) -> alloc::vec::Vec<kdevice::DeviceCore> {
    let ownership = current_device_ownership();
    ownership
        .devices_on_bus(bus)
        .map(|device| kdevice::DeviceCore::new(device.record.id))
        .collect()
}

/// Enumerate current device cores associated with one driver through the `kdriver` facade.
pub fn current_devices_for_driver(
    driver: kdevice::DriverId,
) -> alloc::vec::Vec<kdevice::DeviceCore> {
    let ownership = current_device_ownership();
    ownership
        .devices_for_driver(driver)
        .map(|device| kdevice::DeviceCore::new(device.record.id))
        .collect()
}

fn log_class_summary() {
    let records = kdevice::device_records_snapshot();
    let active = records
        .iter()
        .filter(|r| r.state == kdevice::DeviceState::Active)
        .count();
    debug!("total devices: {} ({} active)", records.len(), active);
}

fn log_device_core_summary() {
    let records = kdevice::device_records_snapshot();
    let mut discovered = 0usize;
    let mut matched = 0usize;
    let mut bound = 0usize;
    let mut active = 0usize;
    let mut removing = 0usize;
    let mut removed = 0usize;

    for record in &records {
        match record.state {
            kdevice::DeviceState::Discovered => discovered += 1,
            kdevice::DeviceState::Matched => matched += 1,
            kdevice::DeviceState::Bound => bound += 1,
            kdevice::DeviceState::Active => active += 1,
            kdevice::DeviceState::Removing => removing += 1,
            kdevice::DeviceState::Removed => removed += 1,
        }
    }

    info!(
        "device core: total={}, discovered={}, matched={}, bound={}, active={}, removing={}, \
         removed={}",
        records.len(),
        discovered,
        matched,
        bound,
        active,
        removing,
        removed
    );

    for record in &records {
        debug!(
            "  device id={:?} state={} kind={} driver={} location={:?} identity={:?}",
            record.id,
            record.state.as_str(),
            record.device_kind.map_or("<unknown>", |kind| kind.as_str()),
            record.driver_name.unwrap_or("<unbound>"),
            record.location,
            record.identity,
        );
    }
}

fn log_device_ownership_summary() {
    let summary = current_device_ownership_summary();
    info!(
        "device ownership: buses={}, drivers={}, devices={}",
        summary.bus_count, summary.driver_count, summary.device_count,
    );
}

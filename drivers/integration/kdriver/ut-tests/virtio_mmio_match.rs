// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Matching and raw-ID classification for VirtIO MMIO drivers.
//!
//! Platform descriptors carry only the `virtio,mmio` firmware identity; the
//! concrete device is decided from a read-only header read during probe.

use driver_base::DeviceKind;
use kdevice::{
    BusTypeId, DeviceDesc, DeviceDescSpec, DeviceIdentity, DeviceLocation, DiscoveryOrigin,
    MatchResult, PciIdentity, PlatformIdentity, ResourceSet, TransportInfo, device_desc_add,
};
use unittest::{assert_eq, def_test};

use super::{MmioMatch, classify_mmio_device, make_virtio_driver};
use crate::driver_registry::virtio::ids::{VIRTIO_MMIO_COMPATIBLE, virtio_type};

fn mmio_desc(bus_id: kdevice::BusId) -> DeviceDesc {
    device_desc_add(DeviceDescSpec {
        bus_id,
        parent: None,
        location: DeviceLocation::Mmio {
            base: 0x1000,
            size: 0x200,
        },
        origin: DiscoveryOrigin::DeviceTree,
        identity: DeviceIdentity::Platform(PlatformIdentity {
            alias: None,
            firmware_id: Some(VIRTIO_MMIO_COMPATIBLE),
        }),
        // Discovery no longer records a transport for MMIO devices.
        transport: None,
        resources: ResourceSet::new(),
        dma: None,
        intx: None,
        firmware_node: None,
    })
    .unwrap()
}

fn pci_desc(bus_id: kdevice::BusId, device_type: u32) -> DeviceDesc {
    device_desc_add(DeviceDescSpec {
        bus_id,
        parent: None,
        location: DeviceLocation::Pci {
            segment: 0,
            bus: 0,
            device: 1,
            function: 0,
        },
        origin: DiscoveryOrigin::DeviceTree,
        identity: DeviceIdentity::Pci(PciIdentity {
            vendor_id: 0x1af4,
            device_id: 0x1042,
            class: 0,
            subclass: 0,
            subsystem_vendor_id: 0,
            subsystem_device_id: 0,
        }),
        transport: Some(TransportInfo::Virtio { device_type }),
        resources: ResourceSet::new(),
        dma: None,
        intx: None,
        firmware_node: None,
    })
    .unwrap()
}

#[def_test]
fn raw_ids_classify_before_any_transport_is_built() {
    assert_eq!(classify_mmio_device(0, virtio_type::BLOCK), MmioMatch::EmptySlot);
    assert_eq!(classify_mmio_device(virtio_type::BLOCK, virtio_type::BLOCK), MmioMatch::Match);
    assert_eq!(
        classify_mmio_device(virtio_type::NET, virtio_type::BLOCK),
        MmioMatch::Mismatch {
            found: virtio_type::NET
        }
    );
    assert_eq!(
        classify_mmio_device(0xffff_ffff, virtio_type::BLOCK),
        MmioMatch::Mismatch { found: 0xffff_ffff }
    );
}

#[def_test(serial)]
fn platform_driver_matches_a_transportless_mmio_descriptor() {
    let (_bus_type, bus) = kdevice::reset_and_setup_platform_bus_for_tests("virtio-mmio-match-bus");
    let desc = mmio_desc(bus.id());
    assert_eq!(desc.transport(), None);

    let driver = make_virtio_driver(
        "virtio-blk-mmio",
        DeviceKind::Block,
        virtio_type::BLOCK,
        BusTypeId::PLATFORM,
    );
    assert_eq!(driver.matcher().matches(&desc), MatchResult::Match);
    let spec = driver
        .matcher()
        .firmware_spec()
        .expect("MMIO driver must expose a firmware spec");
    assert_eq!(spec.dt_compatibles, &[VIRTIO_MMIO_COMPATIBLE]);
    assert!(spec.acpi_ids.is_empty());
}

#[cfg(all(feature = "virtio-rng", feature = "virtio-socket"))]
#[def_test(serial)]
fn wrong_candidate_reads_only_then_the_matching_candidate_activates() -> unittest::TestResult {
    run_wrong_candidate_scenario()
}

/// Probes a rejected candidate before the matching one and leaves no class
/// runtime or range claim behind.
///
/// Shared with the repeated/order test in `virtio_mmio_claim`, which runs this
/// scenario more than once in the same kernel instance.
#[cfg(all(feature = "virtio-rng", feature = "virtio-socket"))]
pub(super) fn run_wrong_candidate_scenario() -> unittest::TestResult {
    use kdevice::{
        ProbeOutcome, ProbeStats, probe_device_desc, register_driver_object, remove_device_managed,
    };

    use super::virtio_mmio_support::{Event, INITIAL_STATUS};

    const FIRST_MMIO_BASE: usize = 0x5b00_0000;
    const FIRST_MMIO_SIZE: usize = 0x200;

    /// Removes the published test device and clears the fixture table even if
    /// an assertion fails. Created right after the fixture is installed, so it
    /// also covers a failure before any device is published.
    struct Cleanup {
        device: Option<kdevice::DeviceId>,
    }

    impl Cleanup {
        fn new() -> Self {
            Self { device: None }
        }

        fn track(&mut self, id: kdevice::DeviceId) {
            self.device = Some(id);
        }

        fn remove(&mut self) {
            if let Some(id) = self.device.take() {
                if let Err(error) = remove_device_managed(id) {
                    log::warn!("failed to remove MMIO chain test device {id:?}: {error:?}");
                }
                // Resetting the device registry cleared kclass's one-shot
                // `Removed` subscription, so the bridge cannot drop the class
                // runtime here. Remove it explicitly to release the transport
                // and its range claim.
                kclass::remove_class_device_for_tests(id);
            }
        }
    }

    impl Drop for Cleanup {
        fn drop(&mut self) {
            self.remove();
            super::virtio_mmio_support::clear();
        }
    }

    let (_bus_type, bus) = kdevice::reset_and_setup_platform_bus_for_tests("virtio-mmio-chain-bus");

    // Register the candidate that must be rejected before the one that owns
    // the device, so the round really probes a wrong candidate first.
    let wrong = register_driver_object(make_virtio_driver(
        "virtio-rng-mmio-chain",
        DeviceKind::Hwrng,
        virtio_type::RNG,
        BusTypeId::PLATFORM,
    ));
    let right = register_driver_object(make_virtio_driver(
        "virtio-vsock-mmio-chain",
        DeviceKind::Vsock,
        virtio_type::VSOCK,
        BusTypeId::PLATFORM,
    ));

    let handle = super::virtio_mmio_support::install(
        FIRST_MMIO_BASE,
        virtio_type::VSOCK,
        virtio::DeviceType::Socket,
    );
    // The fixture is installed: create the cleanup guard before anything else
    // can fail, and attach the published device once it exists.
    let mut cleanup = Cleanup::new();
    let mut resources = ResourceSet::new();
    resources.push(kdevice::ResourceDesc::Mmio(kdevice::MmioRegion::new(
        FIRST_MMIO_BASE,
        FIRST_MMIO_SIZE,
    )));
    let desc = device_desc_add(DeviceDescSpec {
        bus_id: bus.id(),
        parent: None,
        location: DeviceLocation::Mmio {
            base: FIRST_MMIO_BASE,
            size: FIRST_MMIO_SIZE,
        },
        origin: DiscoveryOrigin::DeviceTree,
        identity: DeviceIdentity::Platform(PlatformIdentity {
            alias: None,
            firmware_id: Some(VIRTIO_MMIO_COMPATIBLE),
        }),
        transport: None,
        resources,
        dma: None,
        intx: None,
        firmware_node: None,
    })
    .unwrap();

    // Boot may already have published class devices for the real QEMU
    // devices, so compare against the pre-probe snapshots instead of global
    // emptiness.
    let vsock_before = kclass::vsock_devices().len();
    let hwrng_before = kclass::hwrng_devices().len();
    let dma_before = super::virtio_mmio_support::DmaSnapshot::capture();

    let outcome = probe_device_desc(desc.id()).unwrap();
    let vsock_after = kclass::vsock_devices();
    let hwrng_after = kclass::hwrng_devices();
    // Hand the published device to the guard before the first assertion.
    let records = kdevice::device_records_snapshot();
    let device_id = records
        .first()
        .map(|record| record.id)
        .expect("the matching candidate must publish the probed device");
    cleanup.track(device_id);

    assert_eq!(outcome, ProbeOutcome::Activated);

    // The production HAL layer saw exactly the device's DMA traffic: the 128
    // RX buffers of the real 8192-byte configuration and the two ring blocks
    // of each of the three queues. Nothing was released while the device owns
    // them.
    let hal_after_probe = super::virtio_mmio_support::counts();
    assert_eq!(hal_after_probe.share_calls, 128);
    assert_eq!(hal_after_probe.share_bytes, 128 * 8192);
    assert_eq!(hal_after_probe.alloc_calls, 6);
    assert_eq!(hal_after_probe.unshare_calls, 0);
    assert_eq!(hal_after_probe.dealloc_calls, 0);
    assert_eq!(hal_after_probe.share_failures, 0);
    assert_eq!(hal_after_probe.alloc_failures, 0);

    // The provider, not just the HAL wrapper, holds exactly those mappings.
    // The bounce pool is initialized lazily by this first use, so its backing
    // region is the only coherent allocation allowed on top of the six queue
    // rings; account for it exactly instead of tolerating extra growth.
    let dma_after_probe = super::virtio_mmio_support::DmaSnapshot::capture();
    let probe_dma = dma_after_probe.since(dma_before);
    assert_eq!(probe_dma.active_mappings, 128);
    assert_eq!(probe_dma.active_mapping_bytes, 128 * 8192);
    assert_eq!(
        probe_dma.bounce_used_bytes,
        128 * 8192,
        "the provider must back exactly the RX buffers: {probe_dma:?}"
    );
    let (bounce_backing_allocations, bounce_backing_bytes) =
        dma_after_probe.bounce_backing_growth(dma_before);
    assert_eq!(
        probe_dma.coherent_allocations,
        6 + bounce_backing_allocations,
        "only queue rings and the one-time bounce backing may be live: {probe_dma:?}"
    );
    assert_eq!(
        probe_dma.coherent_bytes,
        6 * 4096 + bounce_backing_bytes,
        "only queue rings and the one-time bounce backing may be live: {probe_dma:?}"
    );

    // Both candidates were really probed once, in registration order, and only
    // the matching one opened a transport.
    assert_eq!(
        wrong.probe_stats(),
        ProbeStats {
            attempts: 1,
            failures: 1,
        }
    );
    assert_eq!(
        right.probe_stats(),
        ProbeStats {
            attempts: 1,
            failures: 0,
        }
    );
    assert_eq!(handle.opens(), 1);
    let events = handle.events();
    assert_eq!(
        events.as_slice(),
        &[Event::Identify, Event::Identify, Event::Open]
    );
    // The header starts with non-zero status bits and no access may clear
    // them: identification is read-only, and only the matching candidate
    // constructs a transport.
    let status_after_identify = handle.status_after_identify();
    assert_eq!(
        status_after_identify.as_slice(),
        &[INITIAL_STATUS, INITIAL_STATUS]
    );
    let status_at_open = handle.status_at_open();
    assert_eq!(status_at_open.as_slice(), &[INITIAL_STATUS]);
    assert_eq!(handle.status(), INITIAL_STATUS);

    assert_eq!(vsock_after.len(), vsock_before + 1);
    // The rejected RNG candidate published no class device.
    assert_eq!(hwrng_after.len(), hwrng_before);
    assert!(wrong.bound_devices_snapshot().is_empty());
    assert_eq!(right.bound_devices_snapshot().len(), 1);

    assert_eq!(records.len(), 1);
    let published = vsock_after
        .iter()
        .find(|device| device.parent().id() == device_id)
        .expect("the matching candidate must publish the probed device");
    assert_eq!(published.driver_name(), "virtio-vsock-mmio-chain");

    // Release the test's own snapshots first, then remove the device and its
    // class entry explicitly. The registry reset cleared kclass's one-shot
    // `Removed` subscription, so `Cleanup` drops the class runtime itself.
    drop(vsock_after);
    drop(hwrng_after);
    drop(records);
    cleanup.remove();
    assert_eq!(kclass::vsock_devices().len(), vsock_before);
    assert_eq!(kclass::hwrng_devices().len(), hwrng_before);
    // Unbind keeps the stable device object and requeues its descriptor.
    let records_after_unbind = kdevice::device_records_snapshot();
    assert_eq!(records_after_unbind.len(), 1);
    assert_eq!(records_after_unbind[0].id, device_id);
    assert_eq!(
        records_after_unbind[0].state,
        kdevice::DeviceState::Discovered
    );
    assert!(records_after_unbind[0].driver_name.is_none());
    assert!(kdevice::find_device(device_id).is_some());
    assert!(right.bound_devices_snapshot().is_empty());

    // The device-owner teardown released every mapping through the real HAL:
    // each share has one unshare and each ring allocation one deallocation.
    let hal_after_remove = super::virtio_mmio_support::counts();
    assert_eq!(hal_after_remove.unshare_calls, hal_after_remove.share_calls);
    assert_eq!(hal_after_remove.unshare_bytes, hal_after_remove.share_bytes);
    assert_eq!(hal_after_remove.dealloc_calls, hal_after_remove.alloc_calls);
    assert_eq!(hal_after_remove.share_failures, 0);
    assert_eq!(hal_after_remove.alloc_failures, 0);

    // The provider's streaming mappings and bounce usage returned to the
    // pre-probe baseline, and the six queue rings were released.
    let dma_after_remove = super::virtio_mmio_support::DmaSnapshot::capture();
    assert_eq!(dma_after_remove.active_mappings, dma_before.active_mappings);
    assert_eq!(
        dma_after_remove.active_mapping_bytes,
        dma_before.active_mapping_bytes
    );
    assert_eq!(
        dma_after_remove.bounce_used_bytes,
        dma_before.bounce_used_bytes
    );
    assert_eq!(
        dma_after_remove.bounce_total_bytes,
        dma_after_probe.bounce_total_bytes
    );
    assert_eq!(
        dma_after_remove.coherent_allocations,
        dma_before.coherent_allocations + bounce_backing_allocations,
        "queue rings must return to the pre-probe baseline, leaving only the bounce backing: {dma_after_remove:?}"
    );
    assert_eq!(
        dma_after_remove.coherent_bytes,
        dma_before.coherent_bytes + bounce_backing_bytes,
        "queue rings must return to the pre-probe baseline, leaving only the bounce backing: {dma_after_remove:?}"
    );

    // The last device register write was the owner's reset to the empty
    // status, after the queues were detached.
    let status_history = handle.status_history();
    assert_eq!(
        status_history.last(),
        Some(&virtio::DeviceStatus::empty())
    );

    // Dropping the class runtime released the transport and its range claim,
    // even though the descriptor and candidate drivers are still registered.
    let reacquired = crate::resource::claim_mmio_region(
        kdevice::MmioRegion::new(FIRST_MMIO_BASE, FIRST_MMIO_SIZE),
        "chain-test-reacquired",
    )
    .unwrap();
    drop(reacquired);

    unittest::TestResult::Ok
}

#[def_test(serial)]
fn pci_driver_still_matches_by_transport_type_only() {
    let (_bus_type, bus) = kdevice::reset_and_setup_platform_bus_for_tests("virtio-pci-match-bus");

    let driver = make_virtio_driver(
        "virtio-blk-pci",
        DeviceKind::Block,
        virtio_type::BLOCK,
        BusTypeId::PCI,
    );
    assert_eq!(
        driver.matcher().matches(&pci_desc(bus.id(), virtio_type::BLOCK)),
        MatchResult::Match
    );
    assert_eq!(
        driver.matcher().matches(&pci_desc(bus.id(), virtio_type::NET)),
        MatchResult::NoMatch
    );
    // A platform MMIO descriptor no longer carries a VirtIO type, so the PCI
    // matcher must not claim it.
    assert_eq!(
        driver.matcher().matches(&mmio_desc(bus.id())),
        MatchResult::NoMatch
    );
    assert!(driver.matcher().firmware_spec().is_none());
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Range claim ordering across the production VirtIO MMIO probe path.
//!
//! One firmware node may be discovered more than once while a live transport
//! still owns its registers. These tests run two distinct descriptors for the
//! same MMIO range through the real claim-before-access sequence and check that
//! the second probe is rejected without touching the device.
//!
//! The last test runs this scenario and the candidate-chain cleanup scenario
//! repeatedly, in both orders, inside one kernel instance. Each scenario must
//! release its class runtime and range claim before the next one starts.

use driver_base::{DeviceKind, DriverError};
use kdevice::{
    BusTypeId, DeviceDescSpec, DeviceIdentity, DeviceLocation, DiscoveryOrigin, MmioRegion,
    PlatformIdentity, ProbeOutcome, ProbeStats, ResourceDesc, ResourceSet, device_desc_add,
    probe_device_desc, register_driver_object, remove_device_managed,
};
use unittest::{assert, assert_eq, def_test};

use super::{make_virtio_driver, virtio_mmio_support};
use crate::driver_registry::virtio::ids::{VIRTIO_MMIO_COMPATIBLE, virtio_type};

const CLAIM_BASE: usize = 0x5a00_0000;
const CLAIM_SIZE: usize = 0x200;

/// Removes the published device and clears the fixture table on every exit.
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
                log::warn!("failed to remove MMIO claim test device {id:?}: {error:?}");
            }
            // Resetting the device registry cleared kclass's one-shot
            // `Removed` subscription, so the bridge cannot drop the class
            // entry here. Withdraw it explicitly; fixture cleanup waits for
            // any remaining readers to release the transport and range claim.
            kclass::remove_class_device_for_tests(id);
        }
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        self.remove();
        virtio_mmio_support::clear();
    }
}

fn mmio_desc(bus_id: kdevice::BusId) -> kdevice::DeviceDesc {
    let mut resources = ResourceSet::new();
    resources.push(ResourceDesc::Mmio(MmioRegion::new(
        CLAIM_BASE,
        CLAIM_SIZE,
    )));
    device_desc_add(DeviceDescSpec {
        bus_id,
        parent: None,
        location: DeviceLocation::Mmio {
            base: CLAIM_BASE,
            size: CLAIM_SIZE,
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
    .unwrap()
}

#[def_test(serial)]
fn live_transport_keeps_the_range_claimed_until_removal() -> unittest::TestResult {
    let _guard = virtio_mmio_support::MMIO_FIXTURE_LOCK.lock();
    run_live_transport_claim_scenario()
}

/// A live transport keeps the range claimed until its final runtime reader
/// releases it after removal; the range is claimable again afterwards.
///
/// Shared with the repeated/order test below, which runs this scenario more
/// than once in the same kernel instance.
pub(super) fn run_live_transport_claim_scenario() -> unittest::TestResult {
    run_claim_scenario(false)
}

#[def_test(serial)]
fn retained_reader_defers_transport_release_after_removal() -> unittest::TestResult {
    let _guard = virtio_mmio_support::MMIO_FIXTURE_LOCK.lock();
    run_claim_scenario(true)
}

fn run_claim_scenario(has_retained_reader: bool) -> unittest::TestResult {
    use super::virtio_mmio_support::{Event, INITIAL_STATUS};

    let (_bus_type, bus) = kdevice::reset_and_setup_platform_bus_for_tests("virtio-mmio-claim-bus");
    kclass::reinstall_event_bridge_for_tests();
    let driver = register_driver_object(make_virtio_driver(
        "virtio-vsock-mmio-claim",
        DeviceKind::Vsock,
        virtio_type::VSOCK,
        BusTypeId::PLATFORM,
    ));
    let handle = super::virtio_mmio_support::install(
        CLAIM_BASE,
        virtio_type::VSOCK,
        virtio::DeviceType::Socket,
    );

    // The guard exists before the first fallible operation so a panic cannot
    // leave the fixture installed or the published device behind.
    let mut cleanup = Cleanup::new();

    let first = mmio_desc(bus.id());
    let second = mmio_desc(bus.id());
    let vsock_before = kclass::vsock_devices().len();
    let dma_before = virtio_mmio_support::DmaSnapshot::capture();

    let outcome = probe_device_desc(first.id()).unwrap();
    // The records snapshot feeds the guard before any assertion can fail.
    // Both descriptors own a stable device object from `device_desc_add`
    // onwards, so select the probed one by its descriptor's device id.
    let records = kdevice::device_records_snapshot();
    let device_id = first.device_id();
    cleanup.track(device_id);
    assert!(
        records.iter().any(|record| record.id == device_id),
        "the first descriptor must publish a device"
    );

    assert_eq!(outcome, ProbeOutcome::Activated);

    // Snapshot the live device's DMA footprint before anything can release it.
    let hal_after_probe = virtio_mmio_support::counts();
    assert_eq!(hal_after_probe.share_calls, 128);
    assert_eq!(hal_after_probe.share_bytes, 128 * 8192);
    assert_eq!(hal_after_probe.alloc_calls, 6);
    assert_eq!(hal_after_probe.unshare_calls, 0);
    assert_eq!(hal_after_probe.dealloc_calls, 0);
    let dma_after_probe = virtio_mmio_support::DmaSnapshot::capture();
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

    assert_eq!(
        driver.probe_stats(),
        ProbeStats {
            attempts: 1,
            failures: 0,
        }
    );
    assert_eq!(handle.opens(), 1);
    let events = handle.events();
    assert_eq!(events.as_slice(), &[Event::Identify, Event::Open]);
    let status_after_identify = handle.status_after_identify();
    assert_eq!(status_after_identify.as_slice(), &[INITIAL_STATUS]);
    let status_at_open = handle.status_at_open();
    assert_eq!(status_at_open.as_slice(), &[INITIAL_STATUS]);
    assert_eq!(handle.status(), INITIAL_STATUS);
    assert_eq!(kclass::vsock_devices().len(), vsock_before + 1);
    assert_eq!(
        records.len(),
        2,
        "each descriptor owns one stable device object"
    );
    let first_record = records
        .iter()
        .find(|record| record.id == device_id)
        .expect("the probed descriptor's record exists");
    assert_eq!(first_record.driver_name, Some("virtio-vsock-mmio-claim"));
    let second_record = records
        .iter()
        .find(|record| record.id == second.device_id())
        .expect("the refused descriptor keeps its stable object");
    assert_eq!(
        second_record.state,
        kdevice::DeviceState::Discovered,
        "the refused probe leaves the stable object unbound"
    );
    assert!(second_record.driver_name.is_none());

    // The owner is alive: a second descriptor for the same firmware node must
    // be refused before the fixture is consulted, so the device sees no second
    // identification or open.
    let outcome = probe_device_desc(second.id()).unwrap();
    assert_eq!(outcome, ProbeOutcome::Unclaimed);
    assert_eq!(
        driver.probe_stats(),
        ProbeStats {
            attempts: 2,
            failures: 1,
        }
    );
    assert_eq!(handle.opens(), 1);
    let events_after_second = handle.events();
    assert_eq!(
        events_after_second.as_slice(),
        &[Event::Identify, Event::Open]
    );
    let status_after_second_identify = handle.status_after_identify();
    assert_eq!(
        status_after_second_identify.as_slice(),
        &[INITIAL_STATUS]
    );
    let status_at_second_open = handle.status_at_open();
    assert_eq!(status_at_second_open.as_slice(), &[INITIAL_STATUS]);
    assert_eq!(handle.status(), INITIAL_STATUS);
    assert_eq!(kclass::vsock_devices().len(), vsock_before + 1);
    let records_after_second = kdevice::device_records_snapshot();
    assert_eq!(
        records_after_second.len(),
        2,
        "both descriptors keep their stable device objects"
    );
    assert_eq!(
        records_after_second
            .iter()
            .filter(|record| record.driver_name == Some("virtio-vsock-mmio-claim"))
            .count(),
        1,
        "the refused descriptor never gains a binding"
    );
    assert_eq!(driver.bound_devices_snapshot().len(), 1);

    // The refused second descriptor did no DMA or queue work either.
    assert_eq!(virtio_mmio_support::counts(), hal_after_probe);
    assert_eq!(
        virtio_mmio_support::DmaSnapshot::capture(),
        dma_after_probe
    );

    // The production provider also refuses a direct claim while the transport
    // owns the range.
    assert!(matches!(
        crate::resource::claim_mmio_region(
            MmioRegion::new(CLAIM_BASE, CLAIM_SIZE),
            "claim-test-direct"
        ),
        Err(DriverError::ResourceBusy)
    ));

    // Removing the class entry withdraws publication, but an in-flight reader
    // can retain the transport and claim. Cleanup removes the entry explicitly
    // because the test registry reset cleared kclass's `Removed` subscription.
    let reader = has_retained_reader.then(|| {
        kclass::vsock_devices()
            .into_iter()
            .find(|device| device.id() == device_id)
            .expect("the available publication has a reader handle")
    });
    drop(records);
    cleanup.remove();
    if reader.is_some() {
        assert!(matches!(
            crate::resource::claim_mmio_region(
                MmioRegion::new(CLAIM_BASE, CLAIM_SIZE),
                "retained-reader-claim",
            ),
            Err(DriverError::ResourceBusy)
        ));
        assert_eq!(virtio_mmio_support::counts().unshare_calls, 0);
    }
    assert_eq!(kclass::vsock_devices().len(), vsock_before);
    // Unbind keeps both stable objects and requeues their descriptors; only
    // unregister (or a registry reset) removes them.
    let records_after_unbind = kdevice::device_records_snapshot();
    assert_eq!(records_after_unbind.len(), 2);
    for record in &records_after_unbind {
        assert_eq!(
            record.state,
            kdevice::DeviceState::Discovered,
            "unbind returns the stable object to the unbound state"
        );
        assert!(
            record.driver_name.is_none(),
            "unbind clears the binding metadata"
        );
    }
    assert!(kdevice::find_device(device_id).is_some());
    assert!(kdevice::find_device(second.device_id()).is_some());
    assert!(driver.bound_devices_snapshot().is_empty());

    let reader_task = reader.map(|reader| {
        // Model a poller preempted while retaining the old publication. Pin
        // its final drop to this CPU so the waiter must allow scheduling.
        let task = ktask::TaskInner::new_kthread(
            move || drop(reader),
            "mmio-retained-reader".into(),
            0x8000,
        )
        .expect("test reader task allocation must succeed");
        let mut mask = ktask::KCpuMask::new();
        mask.set(khal::percpu::this_cpu_id().as_usize(), true);
        task.set_cpumask(mask);
        ktask::spawn_task(task)
    });

    handle.wait_released();
    // Final transport release returned every mapping through the real provider.
    let hal_after_remove = virtio_mmio_support::counts();
    assert_eq!(hal_after_remove.unshare_calls, hal_after_remove.share_calls);
    assert_eq!(hal_after_remove.unshare_bytes, hal_after_remove.share_bytes);
    assert_eq!(hal_after_remove.dealloc_calls, hal_after_remove.alloc_calls);
    assert_eq!(hal_after_remove.share_failures, 0);
    assert_eq!(hal_after_remove.alloc_failures, 0);
    if let Some(task) = reader_task {
        task.join();
    }

    let dma_after_remove = virtio_mmio_support::DmaSnapshot::capture();
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
    let status_history = handle.status_history();
    assert_eq!(
        status_history.last(),
        Some(&virtio::DeviceStatus::empty())
    );
    let reacquired = crate::resource::claim_mmio_region(
        MmioRegion::new(CLAIM_BASE, CLAIM_SIZE),
        "claim-test-reacquired",
    )
    .unwrap();
    drop(reacquired);

    unittest::TestResult::Ok
}

/// Runs both cleanup scenarios repeatedly in one kernel instance and in both
/// orders. A class runtime or range claim left behind by either scenario would
/// make the next probe fail or the final re-claim return `Busy`, so the test
/// does not depend on the runner's global order.
#[cfg(feature = "virtio-rng")]
#[def_test(serial)]
fn scenarios_release_their_ranges_in_both_orders() -> unittest::TestResult {
    let _guard = virtio_mmio_support::MMIO_FIXTURE_LOCK.lock();
    kclass::reinstall_event_bridge_for_tests();
    use super::virtio_mmio_match_unittest::run_wrong_candidate_scenario;

    for _ in 0..2 {
        if matches!(
            run_live_transport_claim_scenario(),
            unittest::TestResult::Failed
        ) || matches!(
            run_wrong_candidate_scenario(),
            unittest::TestResult::Failed
        ) {
            return unittest::TestResult::Failed;
        }
    }
    for _ in 0..2 {
        if matches!(
            run_wrong_candidate_scenario(),
            unittest::TestResult::Failed
        ) || matches!(
            run_live_transport_claim_scenario(),
            unittest::TestResult::Failed
        ) {
            return unittest::TestResult::Failed;
        }
    }
    unittest::TestResult::Ok
}

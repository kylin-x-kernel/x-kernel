// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unit tests for `BindingRuntime`: admission, removal gating, cleanup
//! batches, and the grouped metadata snapshot exposed through
//! `DeviceObject::record_snapshot`.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicUsize, Ordering};

use driver_base::DeviceKind;
use kspin::SpinNoPreempt;
use unittest::{assert_eq, def_test};

use super::BindingRuntime;
use crate::{
    BusId, DeviceId, DeviceIdentity, DeviceLocation, DeviceObject, DeviceState, DiscoveryOrigin,
    DriverId, PlatformIdentity, ResourceSet,
};

#[def_test]
fn binding_runtime_admits_only_active() {
    let binding = BindingRuntime::new();
    assert_eq!(binding.state(), DeviceState::Discovered);
    assert!(!binding.try_acquire());

    binding.mark_matched();
    assert_eq!(binding.state(), DeviceState::Matched);
    assert!(!binding.try_acquire());

    binding.bind(DriverId::new(1), "binding-test", DeviceKind::Block);
    assert_eq!(binding.state(), DeviceState::Bound);
    assert_eq!(binding.driver_id(), Some(DriverId::new(1)));
    assert_eq!(binding.driver_name(), Some("binding-test"));
    assert_eq!(binding.device_kind(), Some(DeviceKind::Block));
    assert!(!binding.try_acquire());

    binding.activate(DeviceKind::Char);
    assert_eq!(binding.state(), DeviceState::Active);
    assert_eq!(binding.device_kind(), Some(DeviceKind::Char));
    assert!(binding.try_acquire());

    binding.release_use();
    assert!(binding.begin_removing());
    assert_eq!(binding.state(), DeviceState::Removing);
    assert!(!binding.try_acquire());

    binding.mark_removed();
    assert_eq!(binding.state(), DeviceState::Removed);
    assert!(!binding.try_acquire());
    assert!(!binding.begin_removing());
}

#[def_test]
fn binding_runtime_busy_preserves_state_and_admission() {
    let binding = BindingRuntime::new();
    binding.bind(DriverId::new(2), "busy-test", DeviceKind::Char);
    binding.activate(DeviceKind::Char);

    assert!(binding.try_acquire());
    assert!(!binding.begin_removing());
    assert_eq!(binding.state(), DeviceState::Active);

    // The failed attempt must restore admission, not leave a transient
    // `Removing` that would reject the next reference.
    assert!(binding.try_acquire());
    binding.release_use();
    binding.release_use();

    assert!(binding.begin_removing());
    assert_eq!(binding.state(), DeviceState::Removing);
    assert!(!binding.begin_removing());
}

#[def_test]
#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
)]
#[allow(
    unknown_lints,
    std_box_in_kernel_tests,
    reason = "CG-049-no-std-box-in-kernel-tests: BindingRuntime::add_cleanup consumes Box<dyn \
              FnOnce() + Send>; these tests exercise its deferred one-shot cleanup contract"
)]
fn cleanup_batch_is_lifo_and_second_drain_is_empty() {
    let binding = BindingRuntime::new();
    let order = Arc::new(SpinNoPreempt::new(Vec::<usize>::new()));
    let next = Arc::new(AtomicUsize::new(0));

    for id in 0..3usize {
        let order = order.clone();
        let next = next.clone();
        binding.add_cleanup(Box::new(move || {
            let seq = next.fetch_add(1, Ordering::Relaxed);
            order.lock().push((seq << 8) | id);
        }));
    }

    binding.run_cleanups();
    assert_eq!(&*order.lock(), &[2, 0x101, 0x200]);

    // No new registrations: a second drain must not re-run the batch.
    binding.run_cleanups();
    assert_eq!(&*order.lock(), &[2, 0x101, 0x200]);
}

#[def_test]
#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
)]
#[allow(
    unknown_lints,
    std_box_in_kernel_tests,
    reason = "CG-049-no-std-box-in-kernel-tests: BindingRuntime::add_cleanup consumes Box<dyn \
              FnOnce() + Send>; these tests exercise its deferred one-shot cleanup contract"
)]
fn cleanup_registered_from_callback_waits_for_next_batch() {
    let binding = Arc::new(BindingRuntime::new());
    let order = Arc::new(SpinNoPreempt::new(Vec::<usize>::new()));

    let outer_order = order.clone();
    let nested_order = order.clone();
    let callback_binding = binding.clone();
    binding.add_cleanup(Box::new(move || {
        outer_order.lock().push(1);
        let nested_order = nested_order.clone();
        // The devres lock is released before callbacks run, so this must not
        // deadlock, and the new cleanup belongs to the next batch.
        callback_binding.add_cleanup(Box::new(move || nested_order.lock().push(2)));
    }));

    binding.run_cleanups();
    assert_eq!(&*order.lock(), &[1]);

    binding.run_cleanups();
    assert_eq!(&*order.lock(), &[1, 2]);
}

#[def_test]
fn binding_snapshot_reads_driver_metadata_as_one_group() {
    let binding = BindingRuntime::new();
    binding.bind(DriverId::new(7), "snapshot-driver", DeviceKind::Net);

    let snapshot = binding.snapshot();
    assert_eq!(snapshot.state, DeviceState::Bound);
    assert_eq!(snapshot.driver_id, Some(DriverId::new(7)));
    assert_eq!(snapshot.driver_name, Some("snapshot-driver"));
    assert_eq!(snapshot.device_kind, Some(DeviceKind::Net));

    binding.activate(DeviceKind::Input);
    let snapshot = binding.snapshot();
    assert_eq!(snapshot.state, DeviceState::Active);
    assert_eq!(snapshot.driver_id, Some(DriverId::new(7)));
    assert_eq!(snapshot.driver_name, Some("snapshot-driver"));
    assert_eq!(snapshot.device_kind, Some(DeviceKind::Input));

    binding.detach();
    let snapshot = binding.snapshot();
    assert_eq!(snapshot.driver_id, None);
    assert_eq!(snapshot.driver_name, None);
    assert_eq!(snapshot.device_kind, None);
    // Detaching metadata is separate from the lifecycle transition.
    assert_eq!(snapshot.state, DeviceState::Active);
}

#[def_test]
fn static_record_maps_binding_and_relations_groups() {
    let device = DeviceObject::new(
        DeviceId::new(11),
        BusId::new(12),
        DeviceLocation::PlatformStatic { id: 13 },
        DiscoveryOrigin::PlatformStatic,
        DeviceIdentity::Platform(PlatformIdentity {
            alias: Some("grouped-test"),
            firmware_id: None,
        }),
        None,
        ResourceSet::new(),
    );
    device.set_parent(Some(DeviceId::new(21)));
    device.set_child_bus(Some(BusId::new(22)));
    device.install_bound_for_test(DriverId::new(31), "grouped-driver", DeviceKind::Char);
    device.mark_active(DeviceKind::Char);

    let record = device.record_snapshot();
    assert_eq!(record.id, DeviceId::new(11));
    assert_eq!(record.bus_id, BusId::new(12));
    assert_eq!(record.parent, Some(DeviceId::new(21)));
    assert_eq!(record.child_bus, Some(BusId::new(22)));
    assert_eq!(record.driver_id, Some(DriverId::new(31)));
    assert_eq!(record.driver_name, Some("grouped-driver"));
    assert_eq!(record.device_kind, Some(DeviceKind::Char));
    assert_eq!(record.state, DeviceState::Active);
}

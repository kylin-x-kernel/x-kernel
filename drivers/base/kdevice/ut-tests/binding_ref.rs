// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unit tests for read-only binding references and specific-binding
//! `DeviceUse` admission.
//!
//! These tests pin the contract that a `BindingRef` keeps the binding runtime
//! alive and snapshots driver identity, but never increments usage: a live
//! reference must not block removal. `DeviceUse` remains the only admission
//! guard and releases exactly the binding instance that admitted it.

use alloc::sync::Arc;

use driver_base::DeviceKind;
use unittest::{assert, assert_eq, def_test};

use crate::{
    BusId, DeviceId, DeviceIdentity, DeviceLocation, DeviceObject, DeviceState, DiscoveryOrigin,
    DriverId, PlatformIdentity, ResourceSet,
};

const DRIVER_ID: DriverId = DriverId::new(43);
const DRIVER_NAME: &str = "binding-ref-driver";

/// Detached fixture with no current binding.
fn unbound_device() -> Arc<DeviceObject> {
    Arc::new(DeviceObject::new(
        DeviceId::new(41),
        BusId::new(0),
        DeviceLocation::PlatformStatic { id: 42 },
        DiscoveryOrigin::PlatformStatic,
        DeviceIdentity::Platform(PlatformIdentity {
            alias: Some("binding-ref-test"),
            firmware_id: None,
        }),
        None,
        ResourceSet::new(),
    ))
}

/// Detached fixture whose slot holds one bound, active runtime.
fn bound_active() -> Arc<DeviceObject> {
    let device = Arc::new(DeviceObject::new_bound_for_test(
        DeviceId::new(41),
        DeviceKind::Char,
    ));
    device.bind_driver(DRIVER_ID, DRIVER_NAME, DeviceKind::Char);
    device.mark_active(DeviceKind::Char);
    device
}

#[def_test]
fn binding_ref_snapshots_identity_before_detach() {
    let device = bound_active();
    let binding = device.binding_ref().expect("bound device captures a ref");

    assert_eq!(binding.driver_id(), DRIVER_ID);
    assert_eq!(binding.driver_name(), DRIVER_NAME);
    assert!(binding.is_active());

    device.detach_driver();
    assert_eq!(device.driver_name(), None);
    assert_eq!(device.driver_id(), None);

    // The handle keeps the historical identity and remains usable even though
    // the parent no longer has driver metadata.
    assert_eq!(binding.driver_id(), DRIVER_ID);
    assert_eq!(binding.driver_name(), DRIVER_NAME);
    assert!(binding.is_active());
    assert!(device.binding_ref().is_none());
}

#[def_test]
fn binding_ref_tracks_lifecycle_without_admission() {
    let device = bound_active();
    let binding = device.binding_ref().expect("bound device captures a ref");
    assert!(binding.is_active());

    // A read-only reference is not an in-use admission, so removal commits
    // without waiting for the handle to go away.
    assert!(device.begin_removing());
    assert_eq!(device.state(), DeviceState::Removing);
    assert!(!binding.is_active());

    device.mark_removed();
    assert_eq!(device.state(), DeviceState::Removed);
    assert!(!binding.is_active());
}

#[def_test]
fn binding_ref_rejects_terminal_binding() {
    let fresh = unbound_device();
    assert!(fresh.binding_ref().is_none());

    let device = bound_active();
    assert!(device.binding_ref().is_some());

    assert!(device.begin_removing());
    assert!(device.binding_ref().is_none());

    device.mark_removed();
    assert!(device.binding_ref().is_none());
}

#[def_test]
fn binding_ref_clones_share_runtime_state() {
    let device = bound_active();
    let first = device.binding_ref().expect("bound device captures a ref");
    let second = first.clone();

    assert_eq!(first.driver_id(), second.driver_id());
    assert_eq!(first.driver_name(), second.driver_name());
    assert!(first.is_active() && second.is_active());

    // Cloning a handle does not create another admission either.
    assert!(device.begin_removing());
    assert!(!first.is_active());
    assert!(!second.is_active());
}

#[def_test]
fn binding_ref_survives_busy_removal_attempt() {
    let device = bound_active();
    let binding = device.binding_ref().expect("bound device captures a ref");
    let in_use = binding
        .try_acquire(&device)
        .expect("active device admits a use");

    // The outstanding DeviceUse, not the BindingRef, is what rejects removal.
    assert!(!device.begin_removing());
    assert_eq!(device.state(), DeviceState::Active);
    assert!(binding.is_active());

    drop(in_use);
    assert!(binding.is_active());
    assert!(device.begin_removing());
    assert!(!binding.is_active());
}

#[def_test]
fn device_use_admission_is_released_once_per_guard() {
    let device = bound_active();
    let binding = device.binding_ref().expect("bound device captures a ref");
    let first = binding
        .try_acquire(&device)
        .expect("active device admits a use");
    let second = binding
        .try_acquire(&device)
        .expect("active device admits a use");

    drop(first);
    // One guard is still alive: removal must back off and restore Active.
    assert!(!device.begin_removing());
    assert_eq!(device.state(), DeviceState::Active);

    drop(second);
    assert!(device.begin_removing());
    assert_eq!(device.state(), DeviceState::Removing);
}

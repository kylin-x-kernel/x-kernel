// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kerrno::KErrorKind;
use kspin::{SpinNoPreempt, static_lock};
use unittest::{assert_eq, def_test};

use super::*;

/// Control implementation that records every request and can be made to fail.
struct RecordingControl {
    calls: AtomicUsize,
    is_failing: AtomicBool,
    requests: SpinNoPreempt<Vec<bool>>,
    order: Arc<SpinNoPreempt<Vec<&'static str>>>,
}

impl RecordingControl {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            is_failing: AtomicBool::new(false),
            requests: SpinNoPreempt::new(Vec::new()),
            order: Arc::new(SpinNoPreempt::new(Vec::new())),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }
}

impl SystemDeviceControl for RecordingControl {
    fn set_online(&self, online: bool) -> KResult<()> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.requests.lock().push(online);
        self.order.lock().push("control");
        if self.is_failing.load(Ordering::Relaxed) {
            return Err(KErrorKind::OperationNotSupported.into());
        }
        Ok(())
    }
}

static_lock! {
    static TEST_LOCK: SpinNoPreempt<()> = SpinNoPreempt::new(());
}

fn cpu_id(index: u32) -> SystemDeviceId {
    SystemDeviceId::new(SystemClass::Cpu, index)
}

fn install_cpu(index: u32, control: Arc<RecordingControl>) -> SystemDeviceId {
    let id = cpu_id(index);
    let _ = install_system_device(SystemDeviceDescriptor::new(id, 0, 0, true, control));
    id
}

#[def_test]
fn ids_round_trip_class_and_index() {
    let _guard = TEST_LOCK.lock();
    let id = SystemDeviceId::new(SystemClass::Memory, 7);
    assert_eq!(id.class(), SystemClass::Memory);
    assert_eq!(id.index(), 7);
    assert_eq!(SystemClass::Cpu.name(), "cpu");
    assert_eq!(SystemClass::Memory.name(), "memory");
}

#[def_test]
fn install_rejects_duplicate_and_remove_rejects_unknown() {
    let _guard = TEST_LOCK.lock();
    reset_system_devices_for_tests();
    let control = Arc::new(RecordingControl::new());
    install_cpu(0, control);
    let duplicate = install_system_device(SystemDeviceDescriptor::new(
        cpu_id(0),
        0,
        0,
        true,
        Arc::new(RecordingControl::new()),
    ));
    assert_eq!(duplicate.unwrap_err(), KErrorKind::AlreadyExists.into());
    assert_eq!(
        remove_system_device(cpu_id(9)).unwrap_err(),
        KErrorKind::NoSuchDevice.into()
    );
    remove_system_device(cpu_id(0)).unwrap();
    assert!(system_device_state(cpu_id(0)).is_none());
}

#[def_test]
fn present_and_online_are_independent() {
    let _guard = TEST_LOCK.lock();
    reset_system_devices_for_tests();
    let control = Arc::new(RecordingControl::new());
    let id = install_cpu(1, control);
    let state = system_device_state(id).unwrap();
    assert!(state.is_present);
    assert!(state.is_online);

    set_system_device_online(id, false).unwrap();
    let state = system_device_state(id).unwrap();
    assert!(state.is_present);
    assert!(!state.is_online);
    assert!(!state.is_going_offline);
    assert_eq!(state.state_name(), "offline");
}

#[def_test]
fn offline_publishes_going_offline_before_the_control_runs() {
    let _guard = TEST_LOCK.lock();
    reset_system_devices_for_tests();
    let control = Arc::new(RecordingControl::new());
    let id = install_cpu(2, control.clone());
    let observed = Arc::new(SpinNoPreempt::new(Vec::new()));
    let order = control.order.clone();
    let sink = observed.clone();
    subscribe_system_device_events(Arc::new(move |event| {
        let is_going_offline = event.state.is_going_offline;
        order.lock().push(match event.kind {
            SystemDeviceEventKind::GoingOffline => "going",
            SystemDeviceEventKind::Offline => "offline",
            _ => "other",
        });
        sink.lock().push((event.kind, is_going_offline));
    }));

    set_system_device_online(id, false).unwrap();
    let events = observed.lock().clone();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].0, SystemDeviceEventKind::GoingOffline);
    assert!(events[0].1);
    assert_eq!(events[1].0, SystemDeviceEventKind::Offline);
    assert!(!events[1].1);
    assert_eq!(*control.order.lock(), alloc::vec!["going", "control", "offline"]);
}

#[def_test]
fn failed_offline_restores_online_state_and_does_not_publish_offline() {
    let _guard = TEST_LOCK.lock();
    reset_system_devices_for_tests();
    let control = Arc::new(RecordingControl::new());
    let id = install_cpu(3, control.clone());
    let observed = Arc::new(SpinNoPreempt::new(Vec::new()));
    let sink = observed.clone();
    subscribe_system_device_events(Arc::new(move |event| {
        sink.lock().push((event.kind, event.state.is_online));
    }));

    control.is_failing.store(true, Ordering::Relaxed);
    let error = set_system_device_online(id, false).unwrap_err();
    assert_eq!(error, KErrorKind::OperationNotSupported.into());

    let state = system_device_state(id).unwrap();
    assert!(state.is_online);
    assert!(state.is_present);
    assert!(!state.is_going_offline);
    assert_eq!(control.calls(), 1);
    let events = observed.lock().clone();
    assert_eq!(events[0].0, SystemDeviceEventKind::GoingOffline);
    assert_eq!(events[1].0, SystemDeviceEventKind::Online);
    assert!(events[1].1);
}

#[def_test]
fn failed_online_keeps_the_device_offline_and_publishes_the_restoring_event() {
    let _guard = TEST_LOCK.lock();
    reset_system_devices_for_tests();
    let control = Arc::new(RecordingControl::new());
    let id = cpu_id(6);
    install_system_device(SystemDeviceDescriptor::new(
        id,
        0,
        0,
        false,
        control.clone(),
    ))
    .unwrap();
    let observed = Arc::new(SpinNoPreempt::new(Vec::new()));
    let sink = observed.clone();
    subscribe_system_device_events(Arc::new(move |event| {
        sink.lock().push((event.kind, event.state.is_online));
    }));

    control.is_failing.store(true, Ordering::Relaxed);
    let error = set_system_device_online(id, true).unwrap_err();
    assert_eq!(error, KErrorKind::OperationNotSupported.into());

    let state = system_device_state(id).unwrap();
    assert!(state.is_present);
    assert!(!state.is_online);
    assert!(!state.is_going_offline);
    assert_eq!(control.calls(), 1);
    let events = observed.lock().clone();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].0, SystemDeviceEventKind::Offline);
    assert!(!events[0].1);
}

#[def_test]
fn removal_allows_a_later_device_with_the_same_id() {
    let _guard = TEST_LOCK.lock();
    reset_system_devices_for_tests();
    let id = cpu_id(7);
    install_cpu(7, Arc::new(RecordingControl::new()));
    remove_system_device(id).unwrap();

    let control = Arc::new(RecordingControl::new());
    install_system_device(SystemDeviceDescriptor::new(
        id,
        0,
        0,
        false,
        control.clone(),
    ))
    .unwrap();
    let state = system_device_state(id).unwrap();
    assert!(state.is_present);
    assert!(!state.is_online);
    set_system_device_online(id, true).unwrap();
    assert_eq!(control.calls(), 1);
    assert!(system_device_state(id).unwrap().is_online);
}

#[def_test]
fn online_is_idempotent_and_does_not_touch_hardware() {
    let _guard = TEST_LOCK.lock();
    reset_system_devices_for_tests();
    let control = Arc::new(RecordingControl::new());
    let id = install_cpu(4, control.clone());
    set_system_device_online(id, true).unwrap();
    assert_eq!(control.calls(), 0);
}

#[def_test]
fn removal_publishes_removed_and_drops_identity() {
    let _guard = TEST_LOCK.lock();
    reset_system_devices_for_tests();
    let control = Arc::new(RecordingControl::new());
    let id = install_cpu(5, control);
    let observed = Arc::new(SpinNoPreempt::new(Vec::new()));
    let sink = observed.clone();
    subscribe_system_device_events(Arc::new(move |event| {
        sink.lock().push((event.kind, event.state.is_present));
    }));

    remove_system_device(id).unwrap();
    assert!(system_device_state(id).is_none());
    let events = observed.lock().clone();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].0, SystemDeviceEventKind::Removed);
    assert!(!events[0].1);
    assert_eq!(system_device_snapshot().len(), 0);
    assert_eq!(present_system_devices(SystemClass::Cpu).len(), 0);
}

#[def_test]
fn snapshot_orders_by_class_then_index() {
    let _guard = TEST_LOCK.lock();
    reset_system_devices_for_tests();
    install_system_device(SystemDeviceDescriptor::new(
        SystemDeviceId::new(SystemClass::Memory, 1),
        0,
        0,
        false,
        Arc::new(RecordingControl::new()),
    ))
    .unwrap();
    install_cpu(1, Arc::new(RecordingControl::new()));
    install_cpu(0, Arc::new(RecordingControl::new()));
    let snapshot = system_device_snapshot();
    assert_eq!(snapshot.len(), 3);
    assert_eq!(snapshot[0].id, cpu_id(0));
    assert_eq!(snapshot[1].id, cpu_id(1));
    assert_eq!(snapshot[2].id.class(), SystemClass::Memory);
    assert!(!snapshot[2].is_online);
    assert_eq!(
        present_system_devices(SystemClass::Cpu),
        alloc::vec![cpu_id(0), cpu_id(1)]
    );
}

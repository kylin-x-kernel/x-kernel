// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device lifecycle event dispatch and state transition notifications.
//!
//! # Subscriber contract
//!
//! Subscriber callbacks registered via `subscribe_device_event_kind`,
//! `subscribe_device_removed`, and the convenience wrappers in
//! [`subscribers`](super::subscribers) run on the thread that issued the
//! triggering lifecycle transition (e.g. inside `probe`, `remove`,
//! `adopt_active_device`). No registry or per-object spinlock is held while
//! they run, but the emitting path does hold the transitioning descriptor's
//! binding lock. They MUST NOT call back into the driver core synchronously:
//!
//! - Do not call `device_registry()` from a subscriber.
//! - Do not invoke `probe_device_desc`, `remove_device_*`,
//!   `adopt_active_device`, or any other mutator on the same thread.
//! - Do not take per-device, per-bus, or per-driver locks already touched by
//!   the current transition.
//!
//! Doing so risks re-entrant access to the lifecycle state machine and is
//! a known source of deadlocks. The recommended pattern is to snapshot the
//! information of interest (`find_device(id)`, `device.identity()`,...) and
//! defer further work to a kernel thread or work item.

use alloc::sync::Arc;

use driver_base::DeviceKind;

use super::event::{DeviceEvent, DeviceEventCallback, DeviceEventKind};
use crate::{DeviceId, DeviceObject, DriverId, device_registry};

/// Register a subscriber for one driver-core lifecycle event kind.
pub fn subscribe_device_event_kind(
    kind: DeviceEventKind,
    callback: Arc<dyn Fn(DeviceEvent) + Send + Sync>,
) {
    device_registry().subscribe_kind(kind, callback);
}

/// Register a callback for device removal events.
pub fn subscribe_device_removed(callback: Arc<dyn Fn(DeviceId) + Send + Sync>) {
    subscribe_device_event_kind(
        DeviceEventKind::Removed,
        Arc::new(move |event| {
            let DeviceEvent::Removed { id } = event else {
                return;
            };
            callback(id);
        }),
    );
}

fn dispatch_event(callbacks: &[DeviceEventCallback], event: DeviceEvent) {
    for callback in callbacks {
        callback(event);
    }
}

pub(crate) fn dispatch_device_event(event: DeviceEvent) {
    // Snapshot only the subscribers registered for this event's kind, so the
    // dispatch path scales with the bucket size rather than the total number
    // of subscribers.
    let callbacks = device_registry().subscribers_for(event.kind());
    dispatch_event(&callbacks, event);
}

/// Dispatch an event through the real subscriber bridge from a test.
///
/// Test-only entry point used to deliver a stale terminal event through the
/// same bridge production uses; tests must not fake class/node teardown by
/// calling internal helpers directly.
#[cfg(unittest)]
pub fn dispatch_device_event_for_tests(event: DeviceEvent) {
    dispatch_device_event(event);
}

/// Silently prepare the `Matched`/`Bound` metadata of an unpublished device.
///
/// No event is dispatched and the object is not yet queryable through the
/// registry when the publish path calls this. Drivers rely on the binding
/// metadata being visible from inside `probe_device` (class construction
/// needs it), while subscribers must only observe it after publication.
pub(crate) fn prepare_device_binding(
    device: &DeviceObject,
    driver_id: DriverId,
    driver_name: &'static str,
    kind: DeviceKind,
) {
    device.mark_matched();
    device.bind_driver(driver_id, driver_name, kind);
}

/// Announce a prepared `Matched` transition after the device is published.
pub(crate) fn announce_device_matched(device: &DeviceObject) {
    dispatch_device_event(DeviceEvent::Matched { id: device.id() });
}

/// Announce a prepared `Bound` transition after the device is published.
pub(crate) fn announce_device_bound(device: &DeviceObject, kind: DeviceKind) {
    dispatch_device_event(DeviceEvent::Bound {
        id: device.id(),
        kind,
    });
}

/// Mark a device active and notify driver-core observers.
pub(crate) fn activate_device(device: &DeviceObject, kind: DeviceKind) {
    device.mark_active(kind);
    dispatch_device_event(DeviceEvent::Activated {
        id: device.id(),
        kind,
    });
}

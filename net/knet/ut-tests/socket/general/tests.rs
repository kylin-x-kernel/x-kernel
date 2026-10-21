// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{sync::Arc, task::Wake};
use core::{sync::atomic::AtomicUsize, task::Context};

use kpoll::{PollRegistrations, PollSet};
use unittest::def_test;

use super::*;

#[derive(Default)]
struct WakeCounter(AtomicUsize);

impl Wake for WakeCounter {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[def_test]
fn rx_registration_rechecks_binding_changes_before_event_subscription() {
    for next_ifindex in [2, 0] {
        let options = GeneralOptions::new();
        options.set_bound_dev_if(1);
        let counter = Arc::new(WakeCounter::default());
        let waker = Waker::from(counter.clone());
        let context = Context::from_waker(&waker);
        let mut registrations = PollRegistrations::new();

        options
            .register_rx_waker_with(&mut registrations.context(&context), |selection, _| {
                assert_eq!(
                    selection,
                    RxDeviceSelection::Interface(NetDeviceId::LOOPBACK)
                );
                // The old device was selected, but the configuration event
                // has not been subscribed yet. Only the recheck catches it.
                options.set_bound_dev_if(next_ifindex);
                Ok(waker.clone())
            })
            .unwrap();
        assert_eq!(counter.0.load(Ordering::Relaxed), 1);
    }
}

#[def_test]
fn binding_changes_rebuild_device_registrations_and_cancel_old_sources() {
    let options = GeneralOptions::new();
    options.set_bound_dev_if(1);
    let devices = [PollSet::new(), PollSet::new()];
    let counter = Arc::new(WakeCounter::default());
    let waker = Waker::from(counter.clone());
    let context = Context::from_waker(&waker);
    let mut registrations = PollRegistrations::new();
    let register_devices = |selection: RxDeviceSelection, context: &mut PollContext<'_>| {
        for (index, device) in devices.iter().enumerate() {
            if selection.includes(NetDeviceId::new(index as i32 + 1).unwrap()) {
                context.register(device)?;
            }
        }
        Ok(waker.clone())
    };

    options
        .register_rx_waker_with(&mut registrations.context(&context), register_devices)
        .unwrap();
    assert_eq!(counter.0.load(Ordering::Relaxed), 0);
    options.set_bound_dev_if(1);
    assert_eq!(counter.0.load(Ordering::Relaxed), 0);
    options.set_bound_dev_if(2);
    assert_eq!(counter.0.load(Ordering::Relaxed), 1);

    options
        .register_rx_waker_with(&mut registrations.context(&context), register_devices)
        .unwrap();
    assert_eq!(devices[0].wake(), 0);
    assert_eq!(devices[1].wake(), 1);
    options.set_bound_dev_if(0);
    assert_eq!(counter.0.load(Ordering::Relaxed), 3);

    options
        .register_rx_waker_with(&mut registrations.context(&context), register_devices)
        .unwrap();
    assert_eq!(devices[0].wake(), 1);
    assert_eq!(devices[1].wake(), 1);

    options
        .register_rx_waker_with(&mut registrations.context(&context), register_devices)
        .unwrap();
    drop(registrations);
    let wakes_before_cancelled_update = counter.0.load(Ordering::Relaxed);
    options.set_bound_dev_if(1);
    assert_eq!(devices[0].wake(), 0);
    assert_eq!(devices[1].wake(), 0);
    assert_eq!(
        counter.0.load(Ordering::Relaxed),
        wakes_before_cancelled_update
    );
}

#[def_test]
fn unchanged_binding_preserves_event_generation() {
    let options = GeneralOptions::new();
    for ifindex in [0, 1, 2, 32, 0] {
        options.set_bound_dev_if(ifindex);
        let generation = options.device_binding_changed.generation();
        options.set_bound_dev_if(ifindex);
        assert!(!options.device_binding_changed.has_changed_since(generation));
        assert_eq!(options.bound_dev_if(), ifindex);
    }
}

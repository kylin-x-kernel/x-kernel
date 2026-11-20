// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec;
use core::sync::atomic::{AtomicBool, Ordering};

use kdevice::{
    BoxedDriver, BusTypeId, DeviceDriver, DeviceId, DeviceKind, DeviceMatcher, DriverId,
    NeverMatcher,
};
use kspin::SpinNoIrq;
use unittest::def_test;

use super::*;

struct TestDriver {
    number: u8,
    must_fail: Arc<AtomicBool>,
    visits: Arc<SpinNoIrq<Vec<u8>>>,
}

impl DeviceDriver for TestDriver {
    type Data = ();
    fn name(&self) -> &'static str {
        "pm-owned-fixture"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Char
    }

    fn bus_types(&self) -> &'static [BusTypeId] {
        &[]
    }

    fn matcher(&self) -> &dyn DeviceMatcher {
        &NeverMatcher
    }

    fn probe_device(&self, _: &kdevice::ProbeContext<'_>) -> DriverResult<()> {
        Ok(())
    }

    fn supports_suspend(&self) -> bool {
        true
    }

    fn suspend(&self, _: &kdevice::PmContext<'_>, _: Option<&mut ()>) -> DriverResult<()> {
        self.visits.lock().push(self.number);
        if self.number == 2 && self.must_fail.load(Ordering::Relaxed) {
            Err(DriverError::ResourceBusy)
        } else {
            Ok(())
        }
    }

    fn resume(&self, _: &kdevice::PmContext<'_>, _: Option<&mut ()>) -> DriverResult<()> {
        self.visits.lock().push(self.number + 10);
        Ok(())
    }
}

#[def_test]
fn failed_device_prefix_recovers_before_a_complete_retry() {
    let visits = Arc::new(SpinNoIrq::new(Vec::with_capacity(12)));
    let must_fail = Arc::new(AtomicBool::new(true));
    let devices = (1..=3)
        .map(|number| {
            let device = Arc::new(DeviceObject::new_bound_for_test(
                DeviceId::new(number as u64),
                DeviceKind::Char,
            ));
            Participant {
                binding: device.binding_ref().unwrap(),
                device,
                driver: Arc::new(DriverObject::new(
                    DriverId::new(0),
                    BoxedDriver::new(TestDriver {
                        number,
                        must_fail: must_fail.clone(),
                        visits: visits.clone(),
                    }),
                )),
                depth: 0,
            }
        })
        .collect();
    let mut power = PowerDevices {
        devices,
        pci: vec![],
        config: None,
        suspended: 0,
        pci_suspended: 0,
    };
    assert_eq!(power.suspend(), Err(DriverError::ResourceBusy));
    power.resume().unwrap();
    assert_eq!(&*visits.lock(), &[1, 2, 11]);
    visits.lock().clear();
    must_fail.store(false, Ordering::Relaxed);
    power.suspend().unwrap();
    power.resume().unwrap();
    power.resume().unwrap();
    assert_eq!(&*visits.lock(), &[1, 2, 3, 13, 12, 11]);
}

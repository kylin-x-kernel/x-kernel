// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unit tests for the hardware RNG device contract.

use alloc::{boxed::Box, sync::Arc, vec::Vec};

use kspin::SpinNoIrq;
use unittest::{assert_eq, def_test};

use super::{completion::*, *};

struct ScriptedCompletion {
    error: Option<DriverError>,
    produce: Box<dyn Fn() + Send + Sync>,
}

impl RngCompletion for ScriptedCompletion {
    fn wait_until(&self, ready: &mut dyn FnMut() -> bool) -> DriverResult {
        if let Some(error) = self.error {
            return Err(error);
        }
        if !ready() {
            (self.produce)();
            if !ready() {
                return Err(DriverError::InvalidInput);
            }
        }
        Ok(())
    }

    fn notify(&self) {} // The scripted host has no parked tasks.
}

struct TestDevice {
    bytes: Arc<SpinNoIrq<Vec<u8>>>,
    completion: Box<dyn RngCompletion>,
}

impl Device for TestDevice {
    fn name(&self) -> &str {
        "test-hwrng"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Hwrng
    }
}

impl HwrngDevice for TestDevice {
    fn is_failed(&self) -> bool {
        false
    }

    fn read(&self, buf: &mut [u8]) -> DriverResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut result = Err(DriverError::WouldBlock);
        self.completion.wait_until(&mut || {
            let mut bytes = self.bytes.lock();
            if bytes.is_empty() {
                result = Err(DriverError::WouldBlock);
                return false;
            }
            let count = buf.len().min(bytes.len());
            buf[..count].copy_from_slice(&bytes[..count]);
            bytes.drain(..count);
            result = Ok(count);
            result != Err(DriverError::WouldBlock)
        })?;
        result
    }
}

fn device(error: Option<DriverError>) -> TestDevice {
    let bytes = Arc::new(SpinNoIrq::new(Vec::new()));
    let completion_bytes = bytes.clone();
    TestDevice {
        bytes,
        completion: Box::new(ScriptedCompletion {
            error,
            produce: Box::new(move || completion_bytes.lock().extend_from_slice(&[7, 9])),
        }),
    }
}

#[def_test]
fn consumer_reads_short_data_through_device_contract() {
    let device = device(None);
    let source: &dyn HwrngDevice = &device;
    let mut buf = [0xcc; 4];
    assert_eq!(source.read(&mut buf), Ok(2));
    assert_eq!(buf, [7, 9, 0xcc, 0xcc]);
    assert_eq!(source.device_kind().as_str(), "hwrng");
}

#[def_test]
fn waiting_errors_do_not_consume_bytes() {
    for error in [DriverError::InvalidInput, DriverError::NoMemory] {
        let device = device(Some(error));
        device.bytes.lock().push(17);
        let mut buf = [0xcc];
        assert_eq!(device.read(&mut []), Ok(0));
        assert_eq!(device.read(&mut buf), Err(error));
        assert_eq!(buf, [0xcc]);
        assert_eq!(&*device.bytes.lock(), &[17]);
    }
}

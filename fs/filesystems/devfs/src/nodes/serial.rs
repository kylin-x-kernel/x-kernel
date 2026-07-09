// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `/dev/serial*` nodes for per-instance UARTs published by the unified
//! serial driver.
//!
//! Each UART bound by the serial driver appears in the char class under a
//! `serial-*` driver name. The stdout UART is additionally adopted into the
//! console subsystem and reached through the static `/dev/console` node, so
//! it is skipped here to avoid exposing the same hardware twice. Remaining
//! (auxiliary) UARTs are exposed as `/dev/serial0`, `/dev/serial1`, ...

use alloc::{format, sync::Arc};
use core::any::Any;

use console_driver::runtime::active_console_id;
use driver_base::DriverError;
use kclass::{CharDevice, CharDeviceImpl, ClassDevice, char_devices};
use kerrno::KError;
use kvfs::{DeviceFileOps, DeviceId, NodeFlags, NodeType, VfsResult};
use kvfs_simple::{DirMapping, SimpleDir, SimpleFs};

use crate::DeviceFile;

/// Linux TTY major; the minor identifies one auxiliary UART.
const SERIAL_MAJOR: u32 = 4;

/// `/dev/serialN` backing for one per-instance UART. Wraps the driver-model
/// `ClassDevice<CharDeviceImpl>` and adapts its offset-less `CharDevice` API
/// to the `DeviceFileOps` the VFS expects.
pub struct SerialDev {
    device: ClassDevice<CharDeviceImpl>,
}

impl SerialDev {
    pub fn new(device: ClassDevice<CharDeviceImpl>) -> Self {
        Self { device }
    }
}

impl DeviceFileOps for SerialDev {
    fn read_at(&self, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        CharDevice::read(&self.device, buf).map_err(map_err)
    }

    fn write_at(&self, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        CharDevice::write(&self.device, buf).map_err(map_err)
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE | NodeFlags::STREAM
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

fn map_err(err: DriverError) -> KError {
    match err {
        DriverError::WouldBlock => KError::WouldBlock,
        _ => KError::Io,
    }
}

/// Build the `/dev/serial*` directory from the char-class snapshot.
///
/// This is a mount-time snapshot (same model as `/dev/input`): UARTs bound
/// after devfs is mounted will not appear. On the current boot flow the
/// serial driver probes during early bus enumeration, before devfs mount, so
/// the auxiliary UARTs are present.
pub fn serial_devices(fs: Arc<SimpleFs>) -> DirMapping {
    let mut serials = DirMapping::new();
    let stdout_id = active_console_id();
    let mut idx = 0u32;
    for device in char_devices() {
        // Only the unified serial driver's per-instance UARTs.
        if !device.driver_name().starts_with("serial-") {
            continue;
        }
        // The stdout UART is already exposed via `/dev/console`; skip it so
        // the same hardware is not published twice.
        if Some(device.id()) == stdout_id {
            continue;
        }
        let dev = DeviceFile::new(
            fs.clone(),
            NodeType::CharacterDevice,
            DeviceId::new(SERIAL_MAJOR, idx),
            Arc::new(SerialDev::new(device)),
        );
        serials.add(format!("serial{idx}"), dev);
        idx += 1;
    }
    serials
}

pub(crate) fn add_root_entries(root: &mut DirMapping, fs: Arc<SimpleFs>) {
    root.add(
        "serial",
        SimpleDir::new_maker(fs.clone(), Arc::new(serial_devices(fs.clone()))),
    );
}

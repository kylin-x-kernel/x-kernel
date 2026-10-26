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

use console_driver::runtime::stdout_console_id;
use driver_base::DriverError;
use kclass::{CharDevice, CharDeviceImpl, ClassDevice, char_devices};
use kerrno::KError;
use kpoll::IoEvents;
use kvfs::{
    DeviceFileOps, DeviceId, DirMapping, NodeFlags, SimpleDir, SimpleFs, VfsFile, VfsFileBuilder,
    VfsInode, VfsResult,
};

use crate::{DeviceFile, add_device_entry};

/// Linux TTY major; auxiliary UART minors start at ttyS0's Linux ABI number.
const SERIAL_MAJOR: u32 = 4;
const SERIAL_MINOR_BASE: u32 = 64;

fn serial_device_id(index: u32) -> DeviceId {
    DeviceId::new(SERIAL_MAJOR, SERIAL_MINOR_BASE + index)
}

/// `/dev/serialN` backing for one per-instance UART. Wraps the driver-model
/// `ClassDevice<CharDeviceImpl>` and adapts its offset-less `CharDevice` API
/// to the `DeviceFileOps` the VFS expects.
pub(crate) struct SerialDev {
    device: ClassDevice<CharDeviceImpl>,
}

impl SerialDev {
    pub(crate) fn new(device: ClassDevice<CharDeviceImpl>) -> Self {
        Self { device }
    }
}

impl DeviceFileOps for SerialDev {
    fn open(&self, _inode: &VfsInode, file: &mut VfsFileBuilder) -> VfsResult<()> {
        // UARTs are non-seekable stream devices: `lseek()` must fail with
        // ESPIPE and reads/writes must ignore file position. `stream_open()`
        // clears `FMode::LSEEK` (so the VFS rejects seeks) and the positional
        // I/O flags, matching POSIX terminal/TTY semantics.
        file.stream_open();
        Ok(())
    }

    fn supports_read(&self) -> bool {
        true
    }

    fn supports_write(&self) -> bool {
        true
    }

    fn read(&self, _file: &VfsFile, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        CharDevice::read(&self.device, buf).map_err(map_err)
    }

    fn write(&self, _file: &VfsFile, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        CharDevice::write(&self.device, buf).map_err(map_err)
    }

    /// Implements `tty_poll` for this node.
    ///
    /// The node is what user space sees for a UART, and Linux gives a UART
    /// `tty_fops`, whose `poll` is what makes `epoll_ctl` accept it; a file
    /// operation set without one would answer `EPERM`, which no Linux UART
    /// does. The UART id here is the Linux ttyS major/minor for the same
    /// reason.
    ///
    /// Readiness is reported as `IN | OUT` because that is what a read and a
    /// write on this node actually do: [`Self::write`] accepts whenever the
    /// port is up, and [`Self::read`] completes without blocking — draining
    /// whatever the UART holds, which for an empty receiver is a zero-length
    /// read, i.e. end of stream. `IN` therefore says "a read returns now", not
    /// "a byte is waiting", and the zero-length answer belongs to the read
    /// contract rather than being a contradiction poll introduces.
    ///
    /// Making `IN` mean "a byte is waiting" — as `n_tty_poll` does with
    /// `input_available_p` — needs three things this node does not have: a
    /// non-destructive receiver query (the raw [`CharDevice`] contract exposes
    /// read/write/flush only, and it consumes), buffered input, and a wake
    /// source. Only the console port has the last two (`prepare_irq_input`
    /// runs for the stdout port alone, and `attach_input_waker` is used by the
    /// console N_TTY), so with the query alone a blocking poll would wait for
    /// input it could never be woken for. Withholding `IN` instead is not the
    /// safer interim: it turns today's immediate zero-length read into a wait
    /// with no wake, where a reader hangs rather than observes the end of
    /// stream the read path already reports.
    fn poll(&self, _file: &VfsFile) -> Option<IoEvents> {
        Some(IoEvents::IN | IoEvents::OUT)
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
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
pub(crate) fn serial_devices(fs: Arc<SimpleFs>) -> DirMapping {
    let mut serials = DirMapping::new();
    let stdout_id = stdout_console_id();
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
        let dev = DeviceFile::new_character(
            fs.clone(),
            serial_device_id(idx),
            Arc::new(SerialDev::new(device)),
        );
        add_device_entry(&mut serials, format!("serial{idx}"), dev);
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

unittest::path_mod_test!(tests, "../../ut-tests/serial.rs");

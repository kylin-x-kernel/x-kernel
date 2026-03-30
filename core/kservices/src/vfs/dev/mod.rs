// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Special devices
#[cfg(feature = "sev")]
mod csv_guest;
#[cfg(all(feature = "dice", target_os = "none"))]
mod dice;
#[cfg(feature = "input")]
mod event;
mod fb;
#[cfg(feature = "dev-log")]
mod log;
mod r#loop;
#[cfg(feature = "memtrack")]
mod memtrack;
mod rtc;
pub mod tty;

use alloc::{format, sync::Arc};
use core::any::Any;

use fs_ng_vfs::{
    DeviceId, Filesystem, NodeFlags, NodeType, ST_NODEV, ST_NOEXEC, ST_NOSUID, ST_RELATIME,
    VfsResult,
};
use kcore::vfs::{Device, DeviceOps, DirMaker, DirMapping, SimpleDir, SimpleFs};
use kerrno::KError;
use ksync::Mutex;
#[cfg(feature = "dev-log")]
pub use log::bind_dev_log;
use rand::{RngCore, SeedableRng, rngs::SmallRng};

const RANDOM_SEED: &[u8; 32] = b"0123456789abcdef0123456789abcdef";

/// Create a new devfs filesystem for device access
pub(crate) fn new_devfs() -> Filesystem {
    SimpleFs::new_with_flags(
        "devfs".into(),
        0x01021994,
        ST_NOSUID | ST_NODEV | ST_NOEXEC | ST_RELATIME,
        builder,
    )
}

/// /dev/null device - discards all writes and returns empty on reads
struct Null;

impl DeviceOps for Null {
    fn read_at(&self, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        Ok(0)
    }

    fn write_at(&self, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Ok(buf.len())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE | NodeFlags::STREAM
    }
}

/// /dev/zero device - returns zero-filled data on reads
struct Zero;

impl DeviceOps for Zero {
    fn read_at(&self, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        buf.fill(0);
        Ok(buf.len())
    }

    fn write_at(&self, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Ok(buf.len())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE | NodeFlags::STREAM
    }
}

/// /dev/random and /dev/urandom device - returns pseudo-random data
struct Random {
    rng: Mutex<SmallRng>,
}

impl Random {
    /// Create a new random device with seeded generator
    pub fn new() -> Self {
        Self {
            rng: Mutex::new(SmallRng::from_seed(*RANDOM_SEED)),
        }
    }
}

impl DeviceOps for Random {
    fn read_at(&self, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        self.rng.lock().fill_bytes(buf);
        Ok(buf.len())
    }

    fn write_at(&self, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Ok(buf.len())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE | NodeFlags::STREAM
    }
}

/// /dev/full device - returns ENOSPC error on writes
struct Full;

impl DeviceOps for Full {
    fn read_at(&self, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        buf.fill(0);
        Ok(buf.len())
    }

    fn write_at(&self, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Err(KError::StorageFull)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE | NodeFlags::STREAM
    }
}

/// /dev/cpu_dma_latency device - controls CPU DMA latency constraints
struct CpuDmaLatency;

impl DeviceOps for CpuDmaLatency {
    fn read_at(&self, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        Err(KError::InvalidInput)
    }

    fn write_at(&self, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Ok(buf.len())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
    }
}

#[cfg(unittest)]
mod dev_tests {
    use unittest::def_test;

    use super::*;

    #[def_test]
    fn test_null_read_returns_zero() {
        let dev = Null;
        let mut buf = [0xFFu8; 64];
        let n = dev.read_at(&mut buf, 0).unwrap();
        assert_eq!(n, 0);
    }

    #[def_test]
    fn test_null_write_accepts_all() {
        let dev = Null;
        let data = [1u8; 128];
        let n = dev.write_at(&data, 0).unwrap();
        assert_eq!(n, 128);
    }

    #[def_test]
    fn test_zero_read_fills_zeros() {
        let dev = Zero;
        let mut buf = [0xFFu8; 32];
        let n = dev.read_at(&mut buf, 0).unwrap();
        assert_eq!(n, 32);
        assert!(buf.iter().all(|&b| b == 0));
    }

    #[def_test]
    fn test_zero_write_accepts_all() {
        let dev = Zero;
        let data = [42u8; 64];
        let n = dev.write_at(&data, 0).unwrap();
        assert_eq!(n, 64);
    }

    #[def_test]
    fn test_full_read_fills_zeros() {
        let dev = Full;
        let mut buf = [0xFFu8; 16];
        let n = dev.read_at(&mut buf, 0).unwrap();
        assert_eq!(n, 16);
        assert!(buf.iter().all(|&b| b == 0));
    }

    #[def_test]
    fn test_full_write_returns_error() {
        let dev = Full;
        let data = [1u8; 8];
        assert!(dev.write_at(&data, 0).is_err());
    }

    #[def_test]
    fn test_random_read_fills_buffer() {
        let dev = Random::new();
        let mut buf = [0u8; 64];
        let n = dev.read_at(&mut buf, 0).unwrap();
        assert_eq!(n, 64);
        assert!(buf.iter().any(|&b| b != 0));
    }

    #[def_test]
    fn test_random_write_accepts_all() {
        let dev = Random::new();
        let data = [0u8; 32];
        let n = dev.write_at(&data, 0).unwrap();
        assert_eq!(n, 32);
    }

    #[def_test]
    fn test_random_two_reads_differ() {
        let dev = Random::new();
        let mut buf1 = [0u8; 32];
        let mut buf2 = [0u8; 32];
        dev.read_at(&mut buf1, 0).unwrap();
        dev.read_at(&mut buf2, 0).unwrap();
        assert_ne!(buf1, buf2);
    }

    #[def_test]
    fn test_cpu_dma_latency_read_error() {
        let dev = CpuDmaLatency;
        let mut buf = [0u8; 4];
        assert!(dev.read_at(&mut buf, 0).is_err());
    }

    #[def_test]
    fn test_cpu_dma_latency_write_ok() {
        let dev = CpuDmaLatency;
        let data = [0u8; 4];
        let n = dev.write_at(&data, 0).unwrap();
        assert_eq!(n, 4);
    }

    #[def_test]
    fn test_null_flags() {
        let dev = Null;
        let flags = dev.flags();
        assert!(flags.contains(NodeFlags::NON_CACHEABLE));
        assert!(flags.contains(NodeFlags::STREAM));
    }

    #[def_test]
    fn test_zero_flags() {
        let dev = Zero;
        let flags = dev.flags();
        assert!(flags.contains(NodeFlags::NON_CACHEABLE));
        assert!(flags.contains(NodeFlags::STREAM));
    }
}

/// Build the devfs filesystem with all standard device entries
fn builder(fs: Arc<SimpleFs>) -> DirMaker {
    let mut root = DirMapping::new();
    root.add(
        "null",
        Device::new(
            fs.clone(),
            NodeType::CharacterDevice,
            DeviceId::new(1, 3),
            Arc::new(Null),
        ),
    );
    root.add(
        "zero",
        Device::new(
            fs.clone(),
            NodeType::CharacterDevice,
            DeviceId::new(1, 5),
            Arc::new(Zero),
        ),
    );
    root.add(
        "full",
        Device::new(
            fs.clone(),
            NodeType::CharacterDevice,
            DeviceId::new(1, 7),
            Arc::new(Full),
        ),
    );
    root.add(
        "random",
        Device::new(
            fs.clone(),
            NodeType::CharacterDevice,
            DeviceId::new(1, 8),
            Arc::new(Random::new()),
        ),
    );
    root.add(
        "urandom",
        Device::new(
            fs.clone(),
            NodeType::CharacterDevice,
            DeviceId::new(1, 9),
            Arc::new(Random::new()),
        ),
    );
    root.add(
        "rtc0",
        Device::new(
            fs.clone(),
            NodeType::CharacterDevice,
            rtc::RTC0_DEVICE_ID,
            Arc::new(rtc::Rtc),
        ),
    );
    if fbdevice::fb_available() {
        root.add(
            "fb0",
            Device::new(
                fs.clone(),
                NodeType::CharacterDevice,
                DeviceId::new(29, 0),
                Arc::new(fb::FrameBuffer::new()),
            ),
        );
    }

    root.add(
        "tty",
        Device::new(
            fs.clone(),
            NodeType::CharacterDevice,
            DeviceId::new(5, 0),
            Arc::new(tty::CurrentTty),
        ),
    );
    root.add(
        "console",
        Device::new(
            fs.clone(),
            NodeType::CharacterDevice,
            DeviceId::new(5, 1),
            tty::N_TTY.clone(),
        ),
    );

    root.add(
        "ptmx",
        Device::new(
            fs.clone(),
            NodeType::CharacterDevice,
            DeviceId::new(5, 2),
            Arc::new(tty::Ptmx(fs.clone())),
        ),
    );
    root.add(
        "pts",
        SimpleDir::new_maker(fs.clone(), Arc::new(tty::PtsDir)),
    );
    #[cfg(feature = "dev-log")]
    root.add(
        "log",
        kcore::vfs::SimpleFile::new(fs.clone(), NodeType::Socket, || Ok(b"")),
    );

    #[cfg(feature = "memtrack")]
    root.add(
        "memtrack",
        Device::new(
            fs.clone(),
            NodeType::CharacterDevice,
            DeviceId::new(114, 514),
            Arc::new(memtrack::MemTrack),
        ),
    );

    root.add(
        "cpu_dma_latency",
        Device::new(
            fs.clone(),
            NodeType::CharacterDevice,
            DeviceId::new(10, 1024),
            Arc::new(CpuDmaLatency),
        ),
    );

    // This is mounted to a tmpfs in `new_procfs`
    root.add(
        "shm",
        SimpleDir::new_maker(fs.clone(), Arc::new(DirMapping::new())),
    );

    // Loop devices
    for i in 0..16 {
        let dev_id = DeviceId::new(7, 0);
        root.add(
            format!("loop{i}"),
            Device::new(
                fs.clone(),
                NodeType::BlockDevice,
                dev_id,
                Arc::new(r#loop::LoopDevice::new(i, dev_id)),
            ),
        );
    }

    // Input devices
    #[cfg(feature = "input")]
    root.add(
        "input",
        SimpleDir::new_maker(fs.clone(), Arc::new(event::input_devices(fs.clone()))),
    );

    #[cfg(all(feature = "dice", target_os = "none"))]
    root.add(
        "dice",
        Device::new(
            fs.clone(),
            NodeType::CharacterDevice,
            DeviceId::new(30, 0),
            Arc::new(dice::DiceNodeInfo::new()),
        ),
    );

    #[cfg(feature = "sev")]
    root.add(
        "csv-guest",
        Device::new(
            fs.clone(),
            NodeType::CharacterDevice,
            DeviceId::new(30, 1),
            Arc::new(csv_guest::CsvGuestDevice::new()),
        ),
    );

    SimpleDir::new_maker(fs, Arc::new(root))
}

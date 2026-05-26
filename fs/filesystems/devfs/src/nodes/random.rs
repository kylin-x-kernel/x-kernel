// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! /dev/random and /dev/urandom device nodes.

use alloc::sync::Arc;
use core::any::Any;

use khal::time::now_ticks;
use ksync::Mutex;
use kvfs::{DeviceFileOps, DeviceId, NodeFlags, NodeType, VfsResult};
use kvfs_simple::{DirMapping, SimpleFs};
use rand::{RngCore, SeedableRng, rngs::SmallRng};

use crate::DeviceFile;

/// /dev/random and /dev/urandom device - returns pseudo-random data.
struct Random {
    rng: Mutex<SmallRng>,
}

impl Random {
    /// Create a new random device seeded from timer entropy.
    pub fn new() -> Self {
        let seed = now_ticks();
        Self {
            rng: Mutex::new(SmallRng::seed_from_u64(seed)),
        }
    }
}

impl DeviceFileOps for Random {
    fn read_at(&self, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        self.rng.lock().fill_bytes(buf);
        Ok(buf.len())
    }

    fn write_at(&self, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        // Writing to /dev/random mixes additional entropy into the PRNG.
        let mut rng = self.rng.lock();
        let mut mix = rng.next_u64();
        for chunk in buf.chunks(8) {
            let mut arr = [0u8; 8];
            arr[..chunk.len()].copy_from_slice(chunk);
            mix ^= u64::from_ne_bytes(arr);
            mix = mix.wrapping_mul(0x517cc1b727220a95);
        }
        drop(rng);
        // Reseed with mixed entropy
        *self.rng.lock() = SmallRng::seed_from_u64(mix);
        Ok(buf.len())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE | NodeFlags::STREAM
    }
}

pub(crate) fn add_root_entries(root: &mut DirMapping, fs: Arc<SimpleFs>) {
    root.add(
        "random",
        DeviceFile::new(
            fs.clone(),
            NodeType::CharacterDevice,
            DeviceId::new(1, 8),
            Arc::new(Random::new()),
        ),
    );
    root.add(
        "urandom",
        DeviceFile::new(
            fs,
            NodeType::CharacterDevice,
            DeviceId::new(1, 9),
            Arc::new(Random::new()),
        ),
    );
}

#[cfg(unittest)]
mod tests {
    use unittest::def_test;

    use super::*;

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
}

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
use rand_chacha::{
    ChaCha20Rng,
    rand_core::{RngCore, SeedableRng},
};

use crate::DeviceFile;

struct RandomState {
    rng: ChaCha20Rng,
    write_counter: u64,
}

impl RandomState {
    fn new() -> Self {
        Self {
            rng: ChaCha20Rng::from_seed(initial_seed()),
            write_counter: 0,
        }
    }

    fn fill_bytes(&mut self, buf: &mut [u8]) {
        self.rng.fill_bytes(buf);
    }

    fn mix_entropy(&mut self, input: &[u8]) {
        let mut seed = [0u8; 32];
        self.rng.fill_bytes(&mut seed);

        for (index, byte) in input.iter().enumerate() {
            seed[index % seed.len()] ^= *byte;
        }

        self.write_counter = self.write_counter.wrapping_add(1);
        let ticks = now_ticks().wrapping_add(self.write_counter.rotate_left(17));
        for (index, byte) in ticks.to_le_bytes().iter().enumerate() {
            seed[index] ^= *byte;
        }

        self.rng = ChaCha20Rng::from_seed(seed);
    }
}

fn initial_seed() -> [u8; 32] {
    let mut seed = [0u8; 32];
    let mut state = now_ticks() ^ 0x9e37_79b9_7f4a_7c15;

    for chunk in seed.chunks_mut(8) {
        state ^= state.rotate_left(7);
        state = state.wrapping_mul(0xd134_2543_de82_ef95);
        chunk.copy_from_slice(&state.to_le_bytes());
    }

    seed
}

/// /dev/random and /dev/urandom device - returns pseudo-random data.
struct Random {
    state: Mutex<RandomState>,
}

impl Random {
    /// Create a new random device seeded from timer entropy.
    pub fn new() -> Self {
        Self {
            state: Mutex::new(RandomState::new()),
        }
    }
}

impl DeviceFileOps for Random {
    fn read_at(&self, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        self.state.lock().fill_bytes(buf);
        Ok(buf.len())
    }

    fn write_at(&self, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        // Writing to /dev/random mixes additional entropy into the PRNG.
        self.state.lock().mix_entropy(buf);
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

    #[def_test]
    fn test_random_write_changes_stream() {
        let dev = Random::new();
        let mut before = [0u8; 32];
        let mut after = [0u8; 32];
        dev.read_at(&mut before, 0).unwrap();
        dev.write_at(b"entropy", 0).unwrap();
        dev.read_at(&mut after, 0).unwrap();
        assert_ne!(before, after);
    }
}

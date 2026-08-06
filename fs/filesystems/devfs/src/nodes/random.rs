// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! /dev/random and /dev/urandom device nodes.

use alloc::sync::Arc;

use entropy::{add_entropy, fill_random};
use kvfs::{
    DeviceFileOps, DeviceId, DirMapping, NodeFlags, NodeType, SimpleFs, VfsFile, VfsResult,
};

use crate::{DeviceFile, add_device_entry};

/// /dev/random and /dev/urandom device nodes backed by the kernel entropy pool.
struct Random;

impl Random {
    /// Create a new random device node.
    pub fn new() -> Self {
        Self
    }

    fn read_bytes(&self, buf: &mut [u8]) -> VfsResult<usize> {
        fill_random(buf);
        Ok(buf.len())
    }

    fn write_bytes(&self, buf: &[u8]) -> VfsResult<usize> {
        // Writing mixes bytes into the pool for diversity but does not credit
        // CRNG readiness (see `entropy::add_entropy`).
        add_entropy(buf);
        Ok(buf.len())
    }
}

impl DeviceFileOps for Random {
    fn supports_read(&self) -> bool {
        true
    }

    fn supports_write(&self) -> bool {
        true
    }

    fn read(&self, _file: &VfsFile, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        self.read_bytes(buf)
    }

    fn write(&self, _file: &VfsFile, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        self.write_bytes(buf)
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
    }
}

pub(crate) fn add_root_entries(root: &mut DirMapping, fs: Arc<SimpleFs>) {
    add_device_entry(
        root,
        "random",
        DeviceFile::new(
            fs.clone(),
            NodeType::CharacterDevice,
            DeviceId::new(1, 8),
            Arc::new(Random::new()),
        ),
    );
    add_device_entry(
        root,
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
    use unittest::{assert, assert_eq, assert_ne, def_test};

    use super::*;

    #[def_test]
    fn test_random_read_fills_buffer() {
        let dev = Random::new();
        let mut buf = [0u8; 64];
        let n = dev.read_bytes(&mut buf).unwrap();
        assert_eq!(n, 64);
        assert!(buf.iter().any(|&b| b != 0));
    }

    #[def_test]
    fn test_random_read_empty() {
        let dev = Random::new();
        let mut buf = [];
        let n = dev.read_bytes(&mut buf).unwrap();
        assert_eq!(n, 0);
    }

    #[def_test]
    fn test_random_write_accepts_all() {
        let dev = Random::new();
        let data = [0u8; 32];
        let n = dev.write_bytes(&data).unwrap();
        assert_eq!(n, 32);
    }

    #[def_test]
    fn test_random_write_empty() {
        let dev = Random::new();
        let n = dev.write_bytes(&[]).unwrap();
        assert_eq!(n, 0);
    }

    #[def_test]
    fn test_random_supports_rw() {
        let dev = Random::new();
        assert!(dev.supports_read());
        assert!(dev.supports_write());
        assert!(dev.flags().contains(NodeFlags::NON_CACHEABLE));
    }

    #[def_test]
    fn test_random_two_reads_differ() {
        let dev = Random::new();
        let mut buf1 = [0u8; 32];
        let mut buf2 = [0u8; 32];
        dev.read_bytes(&mut buf1).unwrap();
        dev.read_bytes(&mut buf2).unwrap();
        assert_ne!(buf1, buf2);
    }

    #[def_test]
    fn test_random_write_changes_stream() {
        let dev = Random::new();
        let mut before = [0u8; 32];
        let mut after = [0u8; 32];
        entropy::init();
        dev.read_bytes(&mut before).unwrap();
        dev.write_bytes(b"entropy").unwrap();
        dev.read_bytes(&mut after).unwrap();
        assert_ne!(before, after);
    }
}

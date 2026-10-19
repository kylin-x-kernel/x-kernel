// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! /dev/random and /dev/urandom device nodes.

use alloc::sync::Arc;

use entropy::{add_entropy, fill_random};
use kvfs::{DeviceFileOps, DeviceId, DirMapping, NodeFlags, SimpleFs, VfsFile, VfsResult};

use crate::{DeviceFile, add_device_entry};

/// /dev/random and /dev/urandom device nodes backed by the kernel entropy pool.
struct Random;

impl Random {
    /// Create a new random device node.
    pub(crate) fn new() -> Self {
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
        DeviceFile::new_character(fs.clone(), DeviceId::new(1, 8), Arc::new(Random::new())),
    );
    add_device_entry(
        root,
        "urandom",
        DeviceFile::new_character(fs, DeviceId::new(1, 9), Arc::new(Random::new())),
    );
}

unittest::path_mod_test!(tests, "../../ut-tests/random.rs");

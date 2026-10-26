// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! /dev/random and /dev/urandom device nodes.

use alloc::sync::Arc;

use entropy::{add_entropy, fill_random};
use kpoll::{IoEvents, PollContext, PollRegisterError};
use kvfs::{DeviceFileOps, DeviceId, DirMapping, NodeFlags, SimpleFs, VfsFile, VfsResult};

use crate::{DeviceFile, add_device_entry};

/// Which of the two entropy `file_operations` sets this node exposes.
///
/// Linux runs both nodes off the same device but gives them different fops:
/// `random_fops` provides `poll`/`random_poll`, `urandom_fops` provides none
/// (`drivers/char/random.c`). That difference is observable to user space —
/// `file_can_poll` makes `epoll_ctl` accept the first and refuse the second —
/// so it belongs to the node rather than to the shared read/write code.
#[derive(Clone, Copy)]
enum Kind {
    /// `/dev/random`: `random_fops`.
    Random,
    /// `/dev/urandom`: `urandom_fops`.
    Urandom,
}

/// `/dev/random` and `/dev/urandom` device nodes backed by the kernel entropy pool.
struct EntropyNode {
    kind: Kind,
}

impl EntropyNode {
    fn new(kind: Kind) -> Self {
        Self { kind }
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

    /// Readiness reported to both wait paths, matching `random_poll` for
    /// `/dev/random` and providing nothing for `/dev/urandom`.
    fn poll_events(&self) -> Option<IoEvents> {
        match self.kind {
            Kind::Urandom => None,
            // `random_poll` reports a seeded pool as readable and an unseeded
            // one as writable, never as "nothing".
            Kind::Random if entropy::is_ready() => Some(IoEvents::IN | IoEvents::RDNORM),
            Kind::Random => Some(IoEvents::OUT | IoEvents::WRNORM),
        }
    }
}

impl DeviceFileOps for EntropyNode {
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

    /// Implements `random_poll` for `/dev/random` only.
    ///
    /// `random_poll` reports `EPOLLIN | EPOLLRDNORM` once the CRNG is seeded and
    /// `EPOLLOUT | EPOLLWRNORM` before that; `/dev/urandom`'s fops provides no
    /// poll operation at all, so it is `EPERM` for `epoll_ctl` and falls back to
    /// always-ready for `poll`/`select`.
    fn poll(&self, _file: &VfsFile) -> Option<IoEvents> {
        self.poll_events()
    }

    /// Waits on the CRNG ready transition, as `random_poll` does on
    /// `crng_init_wait`.
    fn register_poll(
        &self,
        _file: &VfsFile,
        context: &mut PollContext<'_>,
        _events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if matches!(self.kind, Kind::Random) {
            context.register(&entropy::ready_poll_set())?;
        }
        Ok(())
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
    }
}

pub(crate) fn add_root_entries(root: &mut DirMapping, fs: Arc<SimpleFs>) {
    let node = |kind| Arc::new(EntropyNode::new(kind)) as Arc<dyn DeviceFileOps>;
    add_device_entry(
        root,
        "random",
        DeviceFile::new_character(fs.clone(), DeviceId::new(1, 8), node(Kind::Random)),
    );
    add_device_entry(
        root,
        "urandom",
        DeviceFile::new_character(fs, DeviceId::new(1, 9), node(Kind::Urandom)),
    );
}

unittest::path_mod_test!(tests, "../../ut-tests/random.rs");

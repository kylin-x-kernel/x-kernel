// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `io_uring_setup` context creation.

use alloc::sync::Arc;

use kcred::Cred;
use kerrno::KResult;
use kvfs::VfsFile;

use crate::{
    abi::{IoUringCqOffsets, IoUringSqOffsets},
    context::AsyncContext,
    file,
    ring::Ring,
};

/// Outputs that `io_uring_setup` copies back into `struct io_uring_params`.
pub struct IoUringSetup {
    /// Newly created io_uring file.
    pub file: Arc<VfsFile>,
    /// Granted SQ depth.
    pub sq_entries: u32,
    /// Granted CQ depth.
    pub cq_entries: u32,
    /// Advertised feature bits.
    pub features: u32,
    /// SQ ring offsets.
    pub sq_off: IoUringSqOffsets,
    /// CQ ring offsets.
    pub cq_off: IoUringCqOffsets,
}

/// Creates one io_uring context with `entries` SQ slots.
///
/// The caller must be a sleepable user thread. Userspace maps the ring through
/// the returned file's mmap offsets.
///
/// # Errors
///
/// Rejects non-power-of-two or oversized entry counts with `InvalidInput`,
/// and forwards page-allocation and anonymous-file creation errors.
pub fn setup(entries: u32, cred: Arc<Cred>) -> KResult<IoUringSetup> {
    let context = AsyncContext::new()?;
    let ring = Ring::create(entries)?;
    let setup = IoUringSetup {
        sq_entries: ring.sq_entries(),
        cq_entries: ring.cq_entries(),
        features: ring.features(),
        sq_off: ring.sq_offsets(),
        cq_off: ring.cq_offsets(),
        file: file::new_file(context, ring, cred)?,
    };
    Ok(setup)
}

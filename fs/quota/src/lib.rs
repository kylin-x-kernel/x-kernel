// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Filesystem-independent disk quota accounting and v2 quota storage.
//!
//! A filesystem owns one [`QuotaInfo`] and serializes access to it. The
//! filesystem supplies its existing quota-file I/O through [`QuotaIo`]; this
//! crate owns record caching, limits, grace periods and the on-disk radix tree.
//! Quantities are bytes or inode counts, never filesystem blocks. Real time
//! and privilege decisions are supplied by the caller rather than obtained
//! from a current process inside the accounting algorithms.
//!
//! Calls that receive quota I/O may allocate and sleep. They require a live
//! task execution path, but no process identity, and must not run in IRQ or
//! early-boot context or under spin/IRQ-disable/preemption-disable guards.
//! The filesystem context must remain stable throughout an enabled quota type's
//! lifetime; the core retains its canonical backing-file reference. I/O must bypass quota accounting for those
//! files and must not reenter the owning quota state. Filesystem transactions
//! provide crash consistency for multi-write format updates.
#![no_std]

extern crate alloc;

mod accounting;
mod tree;
mod types;
mod v2;

pub use accounting::{Dqinfo, QuotaInfo};
use kerrno::{KError, KResult, LinuxError};
pub use types::{ChargeFlags, Dqblk, QuotaFields, QuotaFormat, QuotaId, QuotaState, QuotaType};

/// Direct I/O using backing-file references owned by one filesystem's quota state.
///
/// The caller serializes metadata access and supplies any transaction required
/// by its filesystem. The mutable write receiver permits borrowing an existing
/// filesystem state and transaction without interior mutability or lock reentry.
/// Each request receives the file retained by `enable`; the provider does not
/// select a file from a second class-to-file registry. Short reads/writes are returned as counts; the format
/// layer turns incomplete records or blocks into I/O errors. A successful
/// write need not be durable until the filesystem commits and synchronizes it.
pub trait QuotaIo {
    /// Canonical backing-file reference retained by the quota state.
    type File;

    /// Reads at a byte offset in the supplied quota file.
    fn read(&self, file: &Self::File, offset: u64, buffer: &mut [u8]) -> KResult<usize>;

    /// Writes at a byte offset, extending the supplied quota file if needed.
    fn write(&mut self, file: &Self::File, offset: u64, buffer: &[u8]) -> KResult<usize>;

    /// Returns the byte size of the supplied quota file.
    fn file_len(&self, file: &Self::File) -> KResult<u64>;
}

fn corrupt() -> KError {
    LinuxError::EUCLEAN.into()
}

fn overflow() -> KError {
    LinuxError::EOVERFLOW.into()
}

fn read_exact<F>(
    io: &mut dyn QuotaIo<File = F>,
    file: &F,
    offset: u64,
    buffer: &mut [u8],
) -> KResult<()> {
    if io.read(file, offset, buffer)? != buffer.len() {
        return Err(LinuxError::EIO.into());
    }
    Ok(())
}

fn write_exact<F>(
    io: &mut dyn QuotaIo<File = F>,
    file: &F,
    offset: u64,
    buffer: &[u8],
) -> KResult<()> {
    if io.write(file, offset, buffer)? != buffer.len() {
        return Err(LinuxError::EIO.into());
    }
    Ok(())
}

unittest::path_mod_test!(tests, "../ut-tests/mod.rs");

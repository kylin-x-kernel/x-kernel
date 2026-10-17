// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `struct seccomp_data` snapshot model.

use super::SECCOMP_DATA_SIZE;

/// Audit architecture value for x86-64 (`AUDIT_ARCH_X86_64`).
pub const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;
/// Audit architecture value for aarch64 (`AUDIT_ARCH_AARCH64`).
pub const AUDIT_ARCH_AARCH64: u32 = 0xc000_00b7;
/// Audit architecture value for riscv64 (`AUDIT_ARCH_RISCV64`).
pub const AUDIT_ARCH_RISCV64: u32 = 0xc000_00f3;
/// Audit architecture value for loongarch64 (`AUDIT_ARCH_LOONGARCH64`).
pub const AUDIT_ARCH_LOONGARCH64: u32 = 0xc000_0102;

/// Byte layout offsets inside a `struct seccomp_data` snapshot.
///
/// These match the Linux userspace ABI (`include/uapi/linux/seccomp.h`):
/// `nr` (i32, offset 0), `arch` (u32, offset 4), `instruction_pointer`
/// (u64, offset 8), `args[6]` (u64 each, offset 16).
/// Offset of the syscall number field (`nr`).
pub const NR_OFFSET: usize = 0;
/// Offset of the `arch` field.
pub const ARCH_OFFSET: usize = 4;
/// Offset of the `instruction_pointer` field.
pub const IP_OFFSET: usize = 8;
/// Offset of the `args[6]` array.
pub const ARGS_OFFSET: usize = 16;

/// A fixed-size `struct seccomp_data` snapshot that classic-BPF programs
/// load through [`crate::run`].
///
/// The struct is kept private-fielded with a single canonical constructor
/// so the on-the-wire layout stays self-consistent across hosts.
#[derive(Clone, Copy, Debug)]
pub struct SeccompData {
    words: [u8; SECCOMP_DATA_SIZE],
}

impl Default for SeccompData {
    fn default() -> Self {
        Self {
            words: [0u8; SECCOMP_DATA_SIZE],
        }
    }
}

impl SeccompData {
    /// Builds a snapshot with the conventional interpretation of each
    /// field. `nr` is treated as `i32` on the wire.
    pub fn new(nr: i32, arch: u32, instruction_pointer: u64, args: [u64; 6]) -> Self {
        let mut words = [0u8; SECCOMP_DATA_SIZE];
        words[NR_OFFSET..NR_OFFSET + 4].copy_from_slice(&nr.to_le_bytes());
        words[ARCH_OFFSET..ARCH_OFFSET + 4].copy_from_slice(&arch.to_le_bytes());
        words[IP_OFFSET..IP_OFFSET + 8].copy_from_slice(&instruction_pointer.to_le_bytes());
        for (i, arg) in args.iter().enumerate() {
            let base = ARGS_OFFSET + i * 8;
            words[base..base + 8].copy_from_slice(&arg.to_le_bytes());
        }
        Self { words }
    }

    /// Returns the raw byte snapshot consumed by classic-BPF loads.
    pub fn as_bytes(&self) -> &[u8] {
        &self.words
    }
}

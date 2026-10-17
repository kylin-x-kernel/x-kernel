// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `struct seccomp_data` layout tests.

use unittest::{assert_eq, def_test};

use super::*;

#[def_test]
fn layout_matches_abi() {
    let data = SeccompData::new(42, AUDIT_ARCH_X86_64, 0xdead_beef, [1, 2, 3, 4, 5, 6]);
    let bytes = data.as_bytes();
    assert_eq!(bytes[NR_OFFSET..NR_OFFSET + 4], 42i32.to_le_bytes());
    assert_eq!(
        bytes[ARCH_OFFSET..ARCH_OFFSET + 4],
        AUDIT_ARCH_X86_64.to_le_bytes()
    );
    assert_eq!(bytes[IP_OFFSET..IP_OFFSET + 8], 0xdead_beefu64.to_le_bytes());
    assert_eq!(bytes[ARGS_OFFSET..ARGS_OFFSET + 8], 1u64.to_le_bytes());
    assert_eq!(bytes[ARGS_OFFSET + 8..ARGS_OFFSET + 16], 2u64.to_le_bytes());
    assert_eq!(
        bytes[ARGS_OFFSET + 40..ARGS_OFFSET + 48],
        6u64.to_le_bytes()
    );
    assert_eq!(bytes.len(), SECCOMP_DATA_SIZE);
}

/// Every `AUDIT_ARCH_*` the syscall layer can report must match Linux's
/// `include/uapi/linux/audit.h`, which is derived from `EM_* | 64BIT | LE`.
#[def_test]
fn audit_arch_constants_match_linux() {
    assert_eq!(AUDIT_ARCH_X86_64, 0xc000_003e); // EM_X86_64 = 62
    assert_eq!(AUDIT_ARCH_AARCH64, 0xc000_00b7); // EM_AARCH64 = 183
    assert_eq!(AUDIT_ARCH_RISCV64, 0xc000_00f3); // EM_RISCV = 243
    assert_eq!(AUDIT_ARCH_LOONGARCH64, 0xc000_0102); // EM_LOONGARCH = 258
}
// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Filter validation and the strict-mode allowlist builder.

use alloc::vec;
use alloc::vec::Vec;

use unittest::{assert, assert_eq, def_test};

use super::*;
use crate::opcode::*;

fn sf(code: u16, jt: u8, jf: u8, k: u32) -> SockFilter {
    SockFilter { code, jt, jf, k }
}

#[def_test]
fn rejects_empty() {
    assert!(matches!(
        SeccompFilter::new(vec![]),
        Err(SeccompFilterError::EmptyProgram)
    ));
}

#[def_test]
fn rejects_too_many() {
    let prog = (0..=BPF_MAXINSNS)
        .map(|_| sf(BPF_RET | BPF_K, 0, 0, 0x7fff_0000))
        .collect::<Vec<_>>();
    assert!(matches!(
        SeccompFilter::new(prog),
        Err(SeccompFilterError::TooManyInstructions)
    ));
}

#[def_test]
fn accepts_max() {
    let prog = (0..BPF_MAXINSNS)
        .map(|_| sf(BPF_RET | BPF_K, 0, 0, 0x7fff_0000))
        .collect::<Vec<_>>();
    assert!(SeccompFilter::new(prog).is_ok());
}

/// An instruction the interpreter cannot execute must be rejected at install
/// time (Linux `seccomp_check_filter`), not left to kill the task later.
#[def_test]
fn rejects_malformed_instructions() {
    // Unknown jump operator.
    assert!(matches!(
        SeccompFilter::new(vec![
            sf(BPF_JMP | 0x50 | BPF_K, 0, 0, 0),
            sf(BPF_RET | BPF_K, 0, 0, 0x7fff_0000),
        ]),
        Err(SeccompFilterError::InvalidInstruction)
    ));

    // Out-of-range jump target.
    assert!(matches!(
        SeccompFilter::new(vec![sf(BPF_JMP | BPF_JEQ | BPF_K, 5, 0, 1)]),
        Err(SeccompFilterError::InvalidInstruction)
    ));

    // Unknown ALU operator.
    assert!(matches!(
        SeccompFilter::new(vec![
            sf(0x04 | 0xf0 | BPF_K, 0, 0, 1),
            sf(BPF_RET | BPF_K, 0, 0, 0x7fff_0000),
        ]),
        Err(SeccompFilterError::InvalidInstruction)
    ));

    // Division by an immediate zero.
    assert!(matches!(
        SeccompFilter::new(vec![
            sf(0x04 | 0x30 | BPF_K, 0, 0, 0),
            sf(BPF_RET | BPF_K, 0, 0, 0x7fff_0000),
        ]),
        Err(SeccompFilterError::InvalidInstruction)
    ));
}

/// The strict allowlist must permit exactly its entries and deny everything
/// else. This is the regression test for the previously malformed program.
#[def_test]
fn strict_allowlist_allows_only_its_entries() {
    // aarch64 syscall numbers for read/write/exit/rt_sigreturn/getpid.
    let allow = [63u32, 64, 93, 139];
    let filter = SeccompFilter::strict_allowlist(&allow).unwrap();

    for nr in allow {
        assert_eq!(
            crate::run(filter.instructions(), &nr_snapshot(nr)),
            0x7fff_0000,
            "allowlisted syscall {nr} must be allowed"
        );
    }
    for nr in [0u32, 1, 172, 56, 999] {
        assert_eq!(
            crate::run(filter.instructions(), &nr_snapshot(nr)),
            0x0000_0000,
            "non-allowlisted syscall {nr} must be killed"
        );
    }
}

fn nr_snapshot(nr: u32) -> alloc::vec::Vec<u8> {
    let mut data = vec![0u8; SECCOMP_DATA_SIZE];
    data[..4].copy_from_slice(&nr.to_le_bytes());
    data
}
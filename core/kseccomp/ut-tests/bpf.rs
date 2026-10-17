// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Classic-BPF interpreter tests, registered from `src/bpf.rs`.


use unittest::def_test;

use super::*;
use crate::opcode::*;
use crate::SockFilter;

fn insn(code: u16, jt: u8, jf: u8, k: u32) -> SockFilter {
    SockFilter { code, jt, jf, k }
}




#[def_test]
fn immediate_load_and_ret() {
    // A = K(0x1234); RET A
    let program = [
        insn(BPF_LD | BPF_W | BPF_IMM, 0, 0, 0x1234),
        insn(BPF_RET | BPF_K, 0, 0, 0x7fff0000), // RET K -> ALLOW regardless of A
    ];
    assert_eq!(run(&program, &[0; SECCOMP_DATA_SIZE]), 0x7fff0000);
}

#[def_test]
fn abs_load_from_seccomp_data_nr() {
    // REPLICATE a filter that reads args[1] and rets what it read.
    let mut data = [0u8; SECCOMP_DATA_SIZE];
    // args[1] lives at offset 24 (16 + 8).
    data[24..28].copy_from_slice(&0xabcdef01u32.to_le_bytes());
    // A = W ABS 24 ; RET A
    let program = [
        insn(BPF_LD | BPF_W | BPF_ABS, 0, 0, 24),
        insn(BPF_RET | SIZE_A, 0, 0, 0),
    ];
    assert_eq!(run(&program, &data), 0xabcdef01);
}

#[def_test]
fn jeq_jump_and_ret_errno() {
    // A = W ABS 0 (syscall nr); if A == 31 (getpid) -> RET ERRNO(1);
    // else RET ALLOW.
    let program = [
        insn(BPF_LD | BPF_W | BPF_ABS, 0, 0, 0),
        insn(BPF_JMP | BPF_JEQ | BPF_K, 0, 1, 31),
        insn(BPF_RET | BPF_K, 0, 0, 0x50001),    // ERRNO(1)
        insn(BPF_RET | BPF_K, 0, 0, 0x7fff0000), // ALLOW
    ];
    let mut data = [0u8; SECCOMP_DATA_SIZE];
    data[0..4].copy_from_slice(&31u32.to_le_bytes());
    assert_eq!(run(&program, &data), 0x50001);
    data[0..4].copy_from_slice(&1u32.to_le_bytes());
    assert_eq!(run(&program, &data), 0x7fff0000);
}

#[def_test]
fn div_by_zero_denies() {
    let program = [
        insn(BPF_LD | BPF_W | BPF_IMM, 0, 0, 10),
        insn(BPF_ALU | BPF_DIV | BPF_K, 0, 0, 0),
        insn(BPF_RET | BPF_K, 0, 0, 0x7fff0000),
    ];
    assert_eq!(run(&program, &[0; SECCOMP_DATA_SIZE]), 0);
}

#[def_test]
fn out_of_range_load_zero_fills() {
    let program = [
        // A = W ABS 4000 (out of seccomp_data bounds) -> 0
        insn(BPF_LD | BPF_W | BPF_ABS, 0, 0, 4000),
        // RET A
        insn(BPF_RET | SIZE_A, 0, 0, 0),
    ];
    assert_eq!(run(&program, &[0; SECCOMP_DATA_SIZE]), 0);
}

#[def_test]
fn mem_and_alu() {
    // M[0] = A(5); A = M[0] + X(2); RET A
    let program = [
        insn(BPF_LD | BPF_W | BPF_IMM, 0, 0, 5),
        insn(BPF_ST, 0, 0, 0),
        insn(BPF_LDX | BPF_W | BPF_IMM, 0, 0, 2),
        insn(BPF_LD | BPF_W | BPF_MEM, 0, 0, 0),
        insn(BPF_ALU | BPF_ADD | BPF_X, 0, 0, 0),
        insn(BPF_RET | SIZE_A, 0, 0, 0),
    ];
    assert_eq!(run(&program, &[0; SECCOMP_DATA_SIZE]), 7);
}

#[def_test]
fn default_deny_on_unsupported_insn() {
    // An unknown jump operator (0x50 is not JEQ/JGT/JGE/JSET/JA) must
    // deny, not crash.
    let program = [
        insn(BPF_JMP | 0x50 | BPF_K, 0, 0, 0), // unsupported jump op
        insn(BPF_RET | BPF_K, 0, 0, 0x7fff0000),
    ];
    assert_eq!(run(&program, &[0; SECCOMP_DATA_SIZE]), 0);
}

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Classic-BPF interpreter for seccomp filters.
//! This is a compact, self-contained classic-BPF (cBPF) machine over a
//! fixed-size byte snapshot ([`super::SeccompData`]). It implements the
//! instruction subset that Linux permits in seccomp filters
//! (`seccomp_check_filter`), so a filter accepted by Linux will run to a
//! deterministic result here.
//!
//! The machine keeps two registers (`A`, `X`), sixteen scratch memory
//! slots (`M[0..16]`), and a program counter. The only volatile input is
//! the data snapshot; there is no side channel, no heap growth, and every
//! load is bounds-checked against the snapshot.

use super::{SECCOMP_DATA_SIZE, SockFilter};
use crate::opcode::*;

/// Loads a 32-bit word from the snapshot.
///
/// The packet is the entire `seccomp_data` snapshot. Linux converts a classic
/// seccomp `BPF_LD|BPF_W|BPF_ABS/IND` into a native-endian context load, so the
/// word is read in the target's ABI order (all supported targets are
/// little-endian). A load whose offset cannot be fully satisfied reads zero.
fn load_word(data: &[u8], offset: usize) -> u32 {
    let Some(bytes) = data.get(offset..offset + 4) else {
        return 0;
    };
    let mut word = [0u8; 4];
    word.copy_from_slice(bytes);
    u32::from_le_bytes(word)
}

/// Loads a 16-bit word from the snapshot in the target's ABI order.
fn load_half(data: &[u8], offset: usize) -> u32 {
    let Some(bytes) = data.get(offset..offset + 2) else {
        return 0;
    };
    let mut half = [0u8; 2];
    half.copy_from_slice(bytes);
    u32::from(u16::from_le_bytes(half))
}

/// Loads a single byte from the snapshot.
fn load_byte(data: &[u8], offset: usize) -> u32 {
    if offset >= data.len() {
        return 0;
    }
    u32::from(data[offset])
}

/// Applies a `BPF_LD`/`BPF_LDX` instruction, returning the value to store.
///
/// Returns `None` for an unsupported addressing mode or size class, which the
/// caller maps to the conservative kill return.
fn load_operand(data: &[u8], code: u16, k: u32, x: u32, mem: &[u32; 16]) -> Option<u32> {
    let load = |offset: usize| -> Option<u32> {
        Some(match size_of(code) {
            BPF_H => load_half(data, offset),
            BPF_B => load_byte(data, offset),
            BPF_W => load_word(data, offset),
            _ => return None,
        })
    };
    match mode_of(code) {
        // Absolute load (BPF_ABS): offset is `k` directly.
        BPF_ABS => load(k as usize),
        // Indexed load (BPF_IND): offset is `x + k`.
        BPF_IND => load(x.wrapping_add(k) as usize),
        BPF_IMM => Some(k),
        BPF_MEM => Some(mem[(k & 0xf) as usize]),
        // The "length" of the packet is the seccomp_data size.
        BPF_LEN => Some(SECCOMP_DATA_SIZE as u32),
        _ => None,
    }
}

/// Applies a `BPF_ALU` instruction, returning the new accumulator.
///
/// Returns `None` for division/modulo by zero or an unsupported operator.
fn alu_operand(op: u16, a: u32, rhs: u32) -> Option<u32> {
    Some(match op {
        BPF_ADD => a.wrapping_add(rhs),
        BPF_SUB => a.wrapping_sub(rhs),
        BPF_MUL => a.wrapping_mul(rhs),
        BPF_DIV => a.checked_div(rhs)?,
        BPF_OR => a | rhs,
        BPF_AND => a & rhs,
        BPF_LSH => a.wrapping_shl(rhs),
        BPF_RSH => a.wrapping_shr(rhs),
        BPF_NEG => a.wrapping_neg(),
        BPF_MOD => a.checked_rem(rhs)?,
        BPF_XOR => a ^ rhs,
        _ => return None,
    })
}

/// Evaluates a classic-BPF seccomp program against the data snapshot.
///
/// Returns the raw `SECCOMP_RET_*` value that the `RET` instruction
/// produced. Malformed programs (out-of-range jumps, division/modulo by
/// zero, unsupported instructions) halt with the conservative
/// `SECCOMP_FILTER_DEFAULT_RETURN` (`SECCOMP_RET_KILL_THREAD`), matching
/// Linux's behavior of never letting a broken filter silently allow a
/// syscall.
pub fn run(program: &[SockFilter], data: &[u8]) -> u32 {
    const DEFAULT_RETURN: u32 = super::decision::SECCOMP_RET_KILL_THREAD;
    // Validation rejects an empty program earlier; an empty one still fails
    // closed instead of panicking.
    if program.is_empty() {
        return DEFAULT_RETURN;
    }

    let mut a: u32 = 0;
    let mut x: u32 = 0;
    let mut mem = [0u32; 16];
    let mut pc: isize = 0;

    // The interpreter is a strict sequential machine that always
    // terminates: jumps reference bounded offsets, and there is no
    // self-modifying state, so the number of steps is bounded by
    // program length. We leave the final bound-check of `pc` to each
    // instruction's `isize` range and rely on the length check below.
    while (0..program.len() as isize).contains(&pc) {
        let insn = &program[pc as usize];
        let code = insn.code;
        let class = class_of(code);
        let k = insn.k;

        match class {
            BPF_LD | BPF_LDX => {
                let Some(value) = load_operand(data, code, k, x, &mem) else {
                    return DEFAULT_RETURN;
                };
                if class == BPF_LD {
                    a = value;
                } else {
                    x = value;
                }
            }
            BPF_ST => {
                mem[(k & 0xf) as usize] = a;
            }
            BPF_STX => {
                mem[(k & 0xf) as usize] = x;
            }
            BPF_ALU => {
                let op = op_of(code);
                let rhs = if src_of(code) == BPF_X { x } else { k };
                let Some(value) = alu_operand(op, a, rhs) else {
                    return DEFAULT_RETURN;
                };
                a = value;
            }
            BPF_JMP => {
                let op = op_of(code);
                if op == BPF_JA {
                    pc += k as isize;
                    continue;
                }
                let rhs = if src_of(code) == BPF_X { x } else { k };
                let taken = match op {
                    BPF_JEQ => a == rhs,
                    BPF_JGT => a > rhs,
                    BPF_JGE => a >= rhs,
                    BPF_JSET => a & rhs != 0,
                    _ => return DEFAULT_RETURN,
                };
                pc += if taken {
                    insn.jt as isize
                } else {
                    insn.jf as isize
                };
            }
            BPF_RET => {
                // In classic BPF the RET "value source" uses the size bits:
                // `BPF_RET|BPF_A` (0x10) returns A, `BPF_RET|BPF_K` returns K.
                // The full 32-bit value is preserved so the errno/data
                // payload (`SECCOMP_RET_DATA`) survives action decoding.
                return if size_of(code) == SIZE_A { a } else { k };
            }
            BPF_MISC => {
                let op = op_of(code) & !BPF_K;
                match op {
                    BPF_TAX => x = a,
                    BPF_TXA => a = x,
                    _ => return DEFAULT_RETURN,
                }
            }
            _ => return DEFAULT_RETURN,
        }
        pc += 1;
    }

    DEFAULT_RETURN
}

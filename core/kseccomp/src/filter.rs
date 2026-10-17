// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Owned seccomp filter programs and their validation.

use alloc::vec::Vec;

use super::{BPF_MAXINSNS, SockFilter};
use crate::{
    decision::{SECCOMP_RET_ALLOW, SECCOMP_RET_KILL_THREAD},
    opcode::*,
};

/// Error returned when a filter program is rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SeccompFilterError {
    /// Program length is zero.
    EmptyProgram,
    /// Program length exceeds [`BPF_MAXINSNS`].
    TooManyInstructions,
    /// An instruction is malformed, or a jump leaves the program. Linux
    /// `seccomp_check_filter` rejects these with `EINVAL` at install time
    /// instead of letting a broken program reach the interpreter.
    InvalidInstruction,
}

/// An owned, validated classic-BPF seccomp program.
#[derive(Debug)]
pub struct SeccompFilter {
    instructions: Vec<SockFilter>,
}

impl SeccompFilter {
    /// Copy-validates a set of classic-BPF instructions read from a user's
    /// `sock_fprog`.
    ///
    /// Mirrors Linux's install-time `seccomp_check_filter`: the length bound
    /// (`BPF_MAXINSNS`) plus an instruction/jump validation pass, so a program
    /// this crate cannot interpret is rejected with `EINVAL` before it is
    /// installed rather than killing the task at the first syscall.
    ///
    /// # Errors
    ///
    /// Returns [`SeccompFilterError::EmptyProgram`] for an empty program,
    /// [`SeccompFilterError::TooManyInstructions`] past [`BPF_MAXINSNS`], and
    /// [`SeccompFilterError::InvalidInstruction`] for a malformed instruction
    /// or an out-of-range jump.
    pub fn new(instructions: Vec<SockFilter>) -> Result<Self, SeccompFilterError> {
        if instructions.is_empty() {
            return Err(SeccompFilterError::EmptyProgram);
        }
        if instructions.len() > BPF_MAXINSNS {
            return Err(SeccompFilterError::TooManyInstructions);
        }
        validate(&instructions)?;
        Ok(Self { instructions })
    }

    /// Returns the validated instruction stream.
    pub fn instructions(&self) -> &[SockFilter] {
        &self.instructions
    }

    /// Builds the kernel's fixed `SECCOMP_MODE_STRICT` allowlist program.
    ///
    /// Linux's strict mode permits exactly the syscalls in `nrs` (the callers
    /// pass `read`, `write`, `_exit`, `rt_sigreturn`). The program returns
    /// `KILL_THREAD` for anything else; the syscall layer maps a strict-mode
    /// denial to `SIGKILL` (Linux `__secure_computing_strict`), while a filter
    /// returning `SECCOMP_RET_KILL_*` delivers `SIGSYS`.
    ///
    /// The program loads the syscall number and compares it against each entry;
    /// a match jumps forward to the shared `ALLOW` return, otherwise control
    /// falls through to `KILL_THREAD`. Classic-BPF jumps are forward-only, so
    /// `ALLOW` sits past the deny return.
    ///
    /// # Errors
    ///
    /// Returns [`SeccompFilterError::TooManyInstructions`] when `nrs` cannot be
    /// encoded in the 8-bit jump offsets of a single program.
    pub fn strict_allowlist(nrs: &[u32]) -> Result<Self, SeccompFilterError> {
        // Each comparison needs a forward jump to the trailing `ALLOW`, whose
        // distance must fit `jt`.
        if nrs.len() > BPF_MAXINSNS || nrs.len() > u8::MAX as usize {
            return Err(SeccompFilterError::TooManyInstructions);
        }

        let mut program = Vec::with_capacity(nrs.len() + 3);
        program.push(insn(
            BPF_LD | BPF_W | BPF_ABS,
            0,
            0,
            super::NR_OFFSET as u32,
        ));
        for (i, nr) in nrs.iter().enumerate() {
            // The `ALLOW` return is at index `nrs.len() + 2`; this comparison
            // sits at `1 + i`, so the taken offset is `nrs.len() - i`.
            let jt = (nrs.len() - i) as u8;
            program.push(insn(BPF_JMP | BPF_JEQ | BPF_K, jt, 0, *nr));
        }
        program.push(insn(BPF_RET | BPF_K, 0, 0, SECCOMP_RET_KILL_THREAD));
        program.push(insn(BPF_RET | BPF_K, 0, 0, SECCOMP_RET_ALLOW));
        Ok(Self {
            instructions: program,
        })
    }
}

const fn insn(code: u16, jt: u8, jf: u8, k: u32) -> SockFilter {
    SockFilter { code, jt, jf, k }
}

/// Validates every instruction and every jump target.
fn validate(instructions: &[SockFilter]) -> Result<(), SeccompFilterError> {
    let len = instructions.len();
    for (pc, insn) in instructions.iter().enumerate() {
        let invalid = || SeccompFilterError::InvalidInstruction;
        let class = insn.code & 0x07;
        let size = insn.code & 0x18;
        let mode = insn.code & 0xe0;
        let op = insn.code & 0xf0;
        match class {
            BPF_LD | BPF_LDX => {
                let ok = match mode {
                    // Loads that do not name a packet location are word-sized.
                    BPF_IMM | BPF_MEM | BPF_LEN => size == BPF_W,
                    BPF_ABS | BPF_IND => matches!(size, BPF_W | BPF_H | BPF_B),
                    _ => false,
                };
                if !ok {
                    return Err(invalid());
                }
            }
            BPF_ST | BPF_STX => {
                if insn.code != class {
                    return Err(invalid());
                }
            }
            BPF_ALU => {
                let known = matches!(
                    op,
                    BPF_ADD
                        | BPF_SUB
                        | BPF_MUL
                        | BPF_DIV
                        | BPF_OR
                        | BPF_AND
                        | BPF_LSH
                        | BPF_RSH
                        | BPF_NEG
                        | BPF_MOD
                        | BPF_XOR
                );
                if !known {
                    return Err(invalid());
                }
                // Division/modulo by an immediate zero can never succeed.
                if matches!(op, BPF_DIV | BPF_MOD) && insn.code & BPF_X == 0 && insn.k == 0 {
                    return Err(invalid());
                }
            }
            BPF_JMP => {
                if op == BPF_JA {
                    if pc + 1 + insn.k as usize >= len {
                        return Err(invalid());
                    }
                } else {
                    if !matches!(op, BPF_JEQ | BPF_JGT | BPF_JGE | BPF_JSET) {
                        return Err(invalid());
                    }
                    if pc + 1 + insn.jt as usize >= len || pc + 1 + insn.jf as usize >= len {
                        return Err(invalid());
                    }
                }
            }
            BPF_RET => {
                if insn.code != BPF_RET | BPF_K && insn.code != BPF_RET | SIZE_A {
                    return Err(invalid());
                }
            }
            BPF_MISC => {
                if !matches!(op & !BPF_X, BPF_TAX | BPF_TXA) {
                    return Err(invalid());
                }
            }
            _ => return Err(invalid()),
        }
    }
    Ok(())
}

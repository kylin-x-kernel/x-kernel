// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Error types for the BPF execution engine.

/// Errors produced by the BPF interpreter and supporting code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// A program byte slice has a length that is not a multiple of the
    /// 8-byte instruction slot size (see [`crate::insn::SLOT_SIZE`]).
    UnalignedProgram,
    /// The interpreter encountered an opcode it does not know how to
    /// execute. Carries the offending opcode byte.
    UnknownOpcode(u8),

    /// An instruction referenced a register index outside the 11-register
    /// eBPF register file (`r0..=r10`).
    InvalidRegister(u8),
    /// A relative jump computed a target slot outside of the program.
    JumpOutOfBounds,
    /// The interpreter reached its per-run instruction limit, which most
    /// likely means the program is looping with no exit path.
    InstructionLimitExceeded,
    /// The program ran past its last instruction without executing an
    /// `exit`.
    EndOfProgram,
    /// A `call` instruction referenced a helper id that is not
    /// registered on this VM. Carries the requested id.
    UnknownHelper(u32),
    /// Attempted to register a new helper id when the helper table was
    /// already full. Replacing an id that is already registered never
    /// fails with this error.
    HelperTableFull,
}

/// Convenience alias for [`core::result::Result`] with this crate's
/// [`Error`] type.
pub type Result<T> = core::result::Result<T, Error>;

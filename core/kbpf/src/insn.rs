// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Decoded representation of a single eBPF instruction slot.
//!
//! The on-wire format of one 8-byte slot is, with all multi-byte fields
//! little-endian:
//!
//! ```text
//! +--------+--------+----------------+--------------------------------+
//! |  opc   |  regs  |     offset     |              imm               |
//! |  u8    |   u8   |     i16        |              i32               |
//! +--------+--------+----------------+--------------------------------+
//! ```
//!
//! The `regs` byte packs the destination and source registers as
//! `(src << 4) | dst`, each register being a 4-bit value in `0..=15`.
//!
//! Some 64-bit immediate instructions (notably `lddw`) consume two
//! consecutive slots; this type only describes a single slot. Composition
//! of multi-slot instructions is left to a higher layer.

/// Size in bytes of one eBPF instruction slot.
pub const SLOT_SIZE: usize = 8;

/// A decoded eBPF instruction slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Insn {
    /// Opcode byte.
    pub opc: u8,
    /// Destination register, in `0..=15`.
    pub dst: u8,
    /// Source register, in `0..=15`.
    pub src: u8,
    /// Signed 16-bit offset (used by branches and memory ops).
    pub off: i16,
    /// Signed 32-bit immediate.
    pub imm: i32,
}

impl Insn {
    /// Decode a single instruction slot from its 8-byte little-endian
    /// wire form.
    ///
    /// This never fails: every 8-byte sequence is a structurally valid
    /// slot. Whether the resulting opcode is one the interpreter knows how
    /// to execute is a separate concern.
    pub const fn from_bytes(bytes: [u8; SLOT_SIZE]) -> Self {
        let regs = bytes[1];
        let off = i16::from_le_bytes([bytes[2], bytes[3]]);
        let imm = i32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        Self {
            opc: bytes[0],
            dst: regs & 0x0F,
            src: (regs >> 4) & 0x0F,
            off,
            imm,
        }
    }
}

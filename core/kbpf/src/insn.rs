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

#[cfg(test)]
mod tests {
    use super::*;

    /// `mov64 r0, 0x12345678` → opc=0xb7, dst=0, src=0, off=0, imm=0x12345678.
    #[test]
    fn decode_mov64_imm_to_r0() {
        let bytes = [0xb7, 0x00, 0x00, 0x00, 0x78, 0x56, 0x34, 0x12];
        let insn = Insn::from_bytes(bytes);
        assert_eq!(insn.opc, 0xb7);
        assert_eq!(insn.dst, 0);
        assert_eq!(insn.src, 0);
        assert_eq!(insn.off, 0);
        assert_eq!(insn.imm, 0x1234_5678);
    }

    /// Verifies dst/src register packing and a negative offset.
    #[test]
    fn decode_regs_and_negative_offset() {
        // regs byte = (src=0x6 << 4) | dst=0x3 = 0x63
        // offset    = -2 = 0xFFFE little-endian = [0xFE, 0xFF]
        let bytes = [0xaa, 0x63, 0xFE, 0xFF, 0x00, 0x00, 0x00, 0x00];
        let insn = Insn::from_bytes(bytes);
        assert_eq!(insn.dst, 0x3);
        assert_eq!(insn.src, 0x6);
        assert_eq!(insn.off, -2);
        assert_eq!(insn.imm, 0);
    }
}

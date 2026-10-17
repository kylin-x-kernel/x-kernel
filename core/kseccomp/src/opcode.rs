// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Classic-BPF instruction encoding (`linux/bpf_common.h`).
//!
//! The constants are grouped by the `code` field they occupy; several share a
//! value because the bit meaning depends on the instruction class.

/// Instruction classes.
pub(crate) const BPF_LD: u16 = 0x00;
pub(crate) const BPF_LDX: u16 = 0x01;
pub(crate) const BPF_ST: u16 = 0x02;
pub(crate) const BPF_STX: u16 = 0x03;
pub(crate) const BPF_ALU: u16 = 0x04;
pub(crate) const BPF_JMP: u16 = 0x05;
pub(crate) const BPF_RET: u16 = 0x06;
pub(crate) const BPF_MISC: u16 = 0x07;

/// Size class within a class byte.
pub(crate) const BPF_W: u16 = 0x00;
pub(crate) const BPF_H: u16 = 0x08;
pub(crate) const BPF_B: u16 = 0x10;

/// `BPF_RET|BPF_A`: return the accumulator rather than `K`.
pub(crate) const SIZE_A: u16 = 0x10;

/// Addressing modes.
pub(crate) const BPF_IMM: u16 = 0x00;
pub(crate) const BPF_ABS: u16 = 0x20;
pub(crate) const BPF_IND: u16 = 0x40;
pub(crate) const BPF_MEM: u16 = 0x60;
pub(crate) const BPF_LEN: u16 = 0x80;

/// ALU and jump operands, and the `X` source bit.
pub(crate) const BPF_K: u16 = 0x00;
pub(crate) const BPF_X: u16 = 0x08;
pub(crate) const BPF_ADD: u16 = 0x00;
pub(crate) const BPF_SUB: u16 = 0x10;
pub(crate) const BPF_MUL: u16 = 0x20;
pub(crate) const BPF_DIV: u16 = 0x30;
pub(crate) const BPF_OR: u16 = 0x40;
pub(crate) const BPF_AND: u16 = 0x50;
pub(crate) const BPF_LSH: u16 = 0x60;
pub(crate) const BPF_RSH: u16 = 0x70;
pub(crate) const BPF_NEG: u16 = 0x80;
pub(crate) const BPF_MOD: u16 = 0x90;
pub(crate) const BPF_XOR: u16 = 0xa0;
pub(crate) const BPF_JA: u16 = 0x00;
pub(crate) const BPF_JEQ: u16 = 0x10;
pub(crate) const BPF_JGT: u16 = 0x20;
pub(crate) const BPF_JGE: u16 = 0x30;
pub(crate) const BPF_JSET: u16 = 0x40;
pub(crate) const BPF_TAX: u16 = 0x00;
pub(crate) const BPF_TXA: u16 = 0x80;

/// Extracts the instruction class from a `code` field.
pub(crate) fn class_of(code: u16) -> u16 {
    code & 0x07
}

/// Extracts the size class from a `code` field.
pub(crate) fn size_of(code: u16) -> u16 {
    code & 0x18
}

/// Extracts the addressing mode from a `code` field.
pub(crate) fn mode_of(code: u16) -> u16 {
    code & 0xe0
}

/// Extracts the ALU/jump operator from a `code` field.
pub(crate) fn op_of(code: u16) -> u16 {
    code & 0xf0
}

/// Extracts the `K`/`X` source bit from a `code` field.
pub(crate) fn src_of(code: u16) -> u16 {
    code & 0x08
}

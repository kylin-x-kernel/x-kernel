// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kbpf::Insn;

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

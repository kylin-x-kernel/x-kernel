// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::sync::Arc;

use kbpf::{Error, ReadOnlyMemory, Vm, vm::InitialRegs};

/// `mov64 r0, 42 ; exit` should leave `r0 = 42`.
#[test]
fn run_returns_immediate() {
    let prog: [u8; 16] = [
        0xb7, 0x00, 0x00, 0x00, 0x2a, 0x00, 0x00, 0x00, // mov64 r0, 42
        0x95, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // exit
    ];
    let mut vm = Vm::new();
    assert_eq!(vm.run(&prog).unwrap(), 42);
}

/// Unknown opcode bytes are surfaced rather than silently ignored.
#[test]
fn run_rejects_unknown_opcode() {
    let prog: [u8; 8] = [0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
    let mut vm = Vm::new();
    assert_eq!(vm.run(&prog), Err(Error::UnknownOpcode(0xff)));
}

/// `mov64 r0, 40 ; add64 r0, 2 ; exit` should leave `r0 = 42`.
#[test]
fn add64_imm_into_r0() {
    let prog: [u8; 24] = [
        0xb7, 0x00, 0x00, 0x00, 0x28, 0x00, 0x00, 0x00, // mov64 r0, 40
        0x07, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, // add64 r0, 2
        0x95, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // exit
    ];
    let mut vm = Vm::new();
    assert_eq!(vm.run(&prog).unwrap(), 42);
}

/// `mov64 r1, 12 ; mov64 r0, 30 ; add64 r0, r1 ; exit` should give 42,
/// exercising both `mov64_reg` and `add64_reg`.
#[test]
fn add64_reg_via_mov64_reg() {
    // The `add64 r0, r1` slot encodes its registers as
    // `(src=1 << 4) | dst=0 = 0x10` in the regs byte.
    let prog: [u8; 32] = [
        0xb7, 0x01, 0x00, 0x00, 0x0c, 0x00, 0x00, 0x00, // mov64 r1, 12
        0xb7, 0x00, 0x00, 0x00, 0x1e, 0x00, 0x00, 0x00, // mov64 r0, 30
        0x0f, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // add64 r0, r1
        0x95, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // exit
    ];
    let mut vm = Vm::new();
    assert_eq!(vm.run(&prog).unwrap(), 42);
}

/// `add64` wraps in two's complement: `(-1) + 1 == 0`.
#[test]
fn add64_wraps_around() {
    let prog: [u8; 24] = [
        0xb7, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, // mov64 r0, -1
        0x07, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, // add64 r0, 1
        0x95, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // exit
    ];
    let mut vm = Vm::new();
    assert_eq!(vm.run(&prog).unwrap(), 0);
}

/// Sum 1..=5 in a `ja` / `jeq_imm` loop and assert `r0 == 15`.
///
/// Layout (each line is one 8-byte slot, slot indices on the left):
///
/// ```text
///   0  mov64 r0, 0           ; accumulator
///   1  mov64 r1, 1           ; counter
///   2  add64 r0, r1          ; loop body: acc += counter
///   3  add64 r1, 1           ; counter += 1
///   4  jeq   r1, 6, +1       ; if counter == 6, skip the back-edge
///   5  ja    -4              ; back-edge to slot 2
///   6  exit
/// ```
#[test]
fn loop_sum_one_to_five() {
    let prog: [u8; 56] = [
        0xb7, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // mov64 r0, 0
        0xb7, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, // mov64 r1, 1
        0x0f, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // add64 r0, r1
        0x07, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, // add64 r1, 1
        0x15, 0x01, 0x01, 0x00, 0x06, 0x00, 0x00, 0x00, // jeq r1, 6, +1
        0x05, 0x00, 0xfc, 0xff, 0x00, 0x00, 0x00, 0x00, // ja -4
        0x95, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // exit
    ];
    let mut vm = Vm::new();
    assert_eq!(vm.run(&prog).unwrap(), 15);
}

/// A branch whose target lies past the end of the program is
/// reported as [`Error::JumpOutOfBounds`].
#[test]
fn jump_out_of_bounds_is_rejected() {
    let prog: [u8; 16] = [
        0x05, 0x00, 0x05, 0x00, 0x00, 0x00, 0x00, 0x00, // ja +5
        0x95, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // exit
    ];
    let mut vm = Vm::new();
    assert_eq!(vm.run(&prog), Err(Error::JumpOutOfBounds));
}

/// `ja -1` jumps back to itself; the limit must catch the runaway
/// loop instead of letting [`Vm::run`] hang.
#[test]
fn instruction_limit_catches_infinite_loop() {
    let prog: [u8; 8] = [
        0x05, 0x00, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, // ja -1
    ];
    let mut vm = Vm::new().with_instruction_limit(100);
    assert_eq!(vm.run(&prog), Err(Error::InstructionLimitExceeded));
}

/// `InitialRegs` should land in `r1..=r5`. Add `r1 + r2` and return
/// it via `r0`.
#[test]
fn initial_regs_seed_r1_to_r5() {
    let prog: [u8; 24] = [
        0xbf, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // mov64 r0, r1
        0x0f, 0x20, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // add64 r0, r2
        0x95, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // exit
    ];
    let mut vm = Vm::new();
    let result = vm
        .run_with_initial_regs(
            &prog,
            InitialRegs {
                r1: 7,
                r2: 35,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(result, 42);
}

/// Two consecutive `run` calls must not see leftover register
/// state from the previous run.
#[test]
fn run_resets_registers_between_runs() {
    let prog: [u8; 16] = [
        0xbf, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // mov64 r0, r1
        0x95, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // exit
    ];
    let mut vm = Vm::new();
    let _first = vm
        .run_with_initial_regs(
            &prog,
            InitialRegs {
                r1: 99,
                ..Default::default()
            },
        )
        .unwrap();
    // Second call uses plain `run`, which seeds zeros.
    assert_eq!(vm.run(&prog).unwrap(), 0);
}

/// `call 1; exit` should invoke the registered helper with
/// `r1..=r5` and place its return value in `r0`.
#[test]
fn call_dispatches_to_registered_helper() {
    fn sum_args(r1: u64, r2: u64, r3: u64, r4: u64, r5: u64) -> u64 {
        r1.wrapping_add(r2)
            .wrapping_add(r3)
            .wrapping_add(r4)
            .wrapping_add(r5)
    }

    let prog: [u8; 16] = [
        0x85, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, // call 1
        0x95, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // exit
    ];

    let mut vm = Vm::new();
    vm.register_helper(1, sum_args).unwrap();

    let result = vm
        .run_with_initial_regs(
            &prog,
            InitialRegs {
                r1: 1,
                r2: 2,
                r3: 4,
                r4: 8,
                r5: 16,
            },
        )
        .unwrap();
    assert_eq!(result, 31);
}

/// `call 7` with no helper bound should surface
/// [`Error::UnknownHelper`].
#[test]
fn call_to_unregistered_helper_is_rejected() {
    let prog: [u8; 16] = [
        0x85, 0x00, 0x00, 0x00, 0x07, 0x00, 0x00, 0x00, // call 7
        0x95, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // exit
    ];
    let mut vm = Vm::new();
    assert_eq!(vm.run(&prog), Err(Error::UnknownHelper(7)));
}

/// A typical `bpf_printk("hi\n")`-style sequence places the format
/// string on the BPF stack, passes its address in `r1`, and calls
/// helper #6 (`bpf_trace_printk`).
#[test]
fn trace_printk_helper_reads_stack_format_string() {
    let prog: [u8; 64] = [
        0x18, 0x01, 0x00, 0x00, 0x68, 0x69, 0x0a, 0x00, // lddw r1, "hi\n\0"
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // high imm32
        0x7b, 0x1a, 0xf8, 0xff, 0x00, 0x00, 0x00, 0x00, // *(u64 *)(r10 - 8) = r1
        0xbf, 0xa1, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // r1 = r10
        0x07, 0x01, 0x00, 0x00, 0xf8, 0xff, 0xff, 0xff, // r1 += -8
        0xb7, 0x02, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, // r2 = 4
        0x85, 0x00, 0x00, 0x00, 0x06, 0x00, 0x00, 0x00, // call bpf_trace_printk
        0x95, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // exit
    ];

    let mut vm = Vm::new();
    assert_eq!(vm.run(&prog).unwrap(), 4);
}

/// Standard libbpf global data references use `BPF_PSEUDO_MAP_VALUE`
/// to pass helper-readable `.rodata` through a map value.
#[test]
fn trace_printk_helper_reads_rodata_map_value() {
    const MAP_ID: u32 = 7;
    let prog: [u8; 40] = [
        0x18, 0x21, 0x00, 0x00, 0x07, 0x00, 0x00, 0x00, // lddw r1, map[7]+0
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // offset 0
        0xb7, 0x02, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, // r2 = 4
        0x85, 0x00, 0x00, 0x00, 0x06, 0x00, 0x00, 0x00, // call bpf_trace_printk
        0x95, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // exit
    ];

    let mut vm = Vm::new();
    vm.add_read_only_memory(ReadOnlyMemory::map_value(MAP_ID, Arc::from(&b"hi\n\0"[..])));
    assert_eq!(vm.run(&prog).unwrap(), 4);
}

/// "Hello, World!" via a print-style helper: the program just calls
/// helper #1, which sees the string buffer through `r1` (pointer)
/// and `r2` (length) and pushes it through a captured channel so
/// the test can verify the bytes round-tripped.
#[test]
fn hello_world_via_call_with_buffer() {
    use std::sync::Mutex;

    // Captured output. A `Mutex<Option<String>>` keeps the helper a
    // plain `fn` (which can't close over local state). We rely on
    // `cargo test` running each test in its own thread for
    // isolation.
    static OUTPUT: Mutex<Option<String>> = Mutex::new(None);

    fn print_helper(ptr: u64, len: u64, _: u64, _: u64, _: u64) -> u64 {
        // SAFETY: the test passes a valid `(ptr, len)` derived from
        // the caller's stack-resident byte slice and keeps it alive
        // for the duration of the call.
        let bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, len as usize) };
        let s = core::str::from_utf8(bytes).unwrap();
        *OUTPUT.lock().unwrap() = Some(s.to_string());
        0
    }

    let msg = b"Hello, World!\n";
    let prog: [u8; 16] = [
        0x85, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, // call 1
        0x95, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // exit
    ];

    let mut vm = Vm::new();
    vm.register_helper(1, print_helper).unwrap();
    let r0 = vm
        .run_with_initial_regs(
            &prog,
            InitialRegs {
                r1: msg.as_ptr() as u64,
                r2: msg.len() as u64,
                ..Default::default()
            },
        )
        .unwrap();

    assert_eq!(r0, 0);
    assert_eq!(OUTPUT.lock().unwrap().as_deref(), Some("Hello, World!\n"));
}

/// Assembles one 8-byte instruction slot.
fn slot(opc: u8, dst: u8, src: u8, off: i16, imm: i32) -> [u8; 8] {
    let mut bytes = [0u8; 8];
    bytes[0] = opc;
    bytes[1] = (src << 4) | dst;
    bytes[2..4].copy_from_slice(&off.to_le_bytes());
    bytes[4..8].copy_from_slice(&imm.to_le_bytes());
    bytes
}

fn slots(insns: &[[u8; 8]]) -> Vec<u8> {
    insns.concat()
}

const OP_LDX_W: u8 = 0x61;
const OP_ALU32_AND_IMM: u8 = 0x54;
const OP_ALU32_RSH_IMM: u8 = 0x74;
const OP_ALU32_MOV_IMM: u8 = 0xb4;
const OP_ALU32_MOV_REG: u8 = 0xbc;
const OP_DIV32_IMM: u8 = 0x34;
const OP_MOD32_IMM: u8 = 0x94;
const OP_JNE_IMM: u8 = 0x55;
const OP_JLT_IMM: u8 = 0xa5;
const OP_JSGT_IMM: u8 = 0x65;
const OP_JEQ32_IMM: u8 = 0x16;
const OP_JNE32_IMM: u8 = 0x56;
const OP_JA: u8 = 0x05;

/// A runc `libcontainer/cgroups/ebpf/devicefilter`-shaped program: the
/// context is `bpf_cgroup_dev_ctx { access_type, major, minor }` (with
/// `access_type = (access bits << 16) | device type`, Linux
/// `kernel/bpf/cgroup.c`'s encoding) and the program allows (returns 1)
/// exactly one char-device write.
fn runc_style_device_filter() -> Vec<u8> {
    // char device | write access under Linux's access_type encoding.
    const CHAR_WRITE: i32 = 0x0004_0002;
    slots(&[
        // r3 = ctx->access_type
        slot(OP_LDX_W, 3, 1, 0, 0),
        // runc's single-mask rule check: (access_type & (type | access)) must
        // keep every bit of (char-device type | write access), otherwise the
        // rule does not apply and the access is denied.
        slot(OP_ALU32_MOV_REG, 2, 3, 0, 0),
        slot(OP_ALU32_AND_IMM, 2, 0, 0, CHAR_WRITE),
        slot(OP_JNE32_IMM, 2, 0, 6, CHAR_WRITE),
        // major must be 1
        slot(OP_LDX_W, 2, 1, 4, 0),
        slot(OP_JNE_IMM, 2, 0, 4, 1),
        // minor must be 3
        slot(OP_LDX_W, 2, 1, 8, 0),
        slot(OP_JNE_IMM, 2, 0, 2, 3),
        // allow
        slot(OP_ALU32_MOV_IMM, 0, 0, 0, 1),
        slot(0x95, 0, 0, 0, 0),
        // deny
        slot(OP_ALU32_MOV_IMM, 0, 0, 0, 0),
        slot(0x95, 0, 0, 0, 0),
    ])
}

fn run_device_filter(access: i32, major: i32, minor: i32) -> u64 {
    let mut ctx = [0u8; 12];
    ctx[0..4].copy_from_slice(&access.to_le_bytes());
    ctx[4..8].copy_from_slice(&major.to_le_bytes());
    ctx[8..12].copy_from_slice(&minor.to_le_bytes());
    let (ctx_memory, ctx_base) = ReadOnlyMemory::context(Arc::new(ctx));
    let mut vm = Vm::new();
    vm.add_read_only_memory(ctx_memory);
    vm.run_with_initial_regs(
        &runc_style_device_filter(),
        InitialRegs {
            r1: ctx_base,
            ..InitialRegs::default()
        },
    )
    .unwrap()
}

/// The interpreted device filter grants only the exact (access, type,
/// major, minor) tuple the rules describe and denies everything else — the
/// behaviour a container's device policy depends on.
#[test]
fn runc_style_device_filter_decides_access() {
    // Linux encodes access_type as (access << 16) | device type:
    // char=2, block=1; read=2<<16, write=4<<16.
    const CHAR_WRITE: i32 = 0x0004_0002;
    const CHAR_READ: i32 = 0x0002_0002;
    const BLOCK_WRITE: i32 = 0x0004_0001;

    assert_eq!(
        run_device_filter(CHAR_WRITE, 1, 3),
        1,
        "the matching write access must be allowed"
    );
    assert_eq!(
        run_device_filter(CHAR_READ, 1, 3),
        0,
        "a non-matching access bit must be denied"
    );
    assert_eq!(
        run_device_filter(BLOCK_WRITE, 1, 3),
        0,
        "a different device type must be denied"
    );
    assert_eq!(
        run_device_filter(CHAR_WRITE, 1, 4),
        0,
        "a different minor must be denied"
    );
    assert_eq!(
        run_device_filter(CHAR_WRITE, 2, 3),
        0,
        "a different major must be denied"
    );
}

/// 32-bit ALU operations zero-extend into the register: the upper word of a
/// 64-bit value must not leak through a `mov32`/`and32` sequence.
#[test]
fn alu32_results_zero_extend_the_register() {
    // r1 = 0x1_0000_0002 ; r0 = mov32(r1) ; r0 &= 3 -> 2
    let prog = slots(&[
        slot(0x18, 1, 0, 0, 2), // lddw r1 = 0x00000001_00000002
        slot(0x00, 0, 0, 0, 1),
        slot(OP_ALU32_MOV_REG, 0, 1, 0, 0), // mov32 r0, r1
        slot(OP_ALU32_AND_IMM, 0, 0, 0, 3), // and32 r0, 3
        slot(0x95, 0, 0, 0, 0),             // exit
    ]);
    let mut vm = Vm::new();
    assert_eq!(vm.run(&prog).unwrap(), 2);
}

/// 32-bit shifts shift the low word only, and the shift count is masked to
/// the width — the interpreter's explicit `& 31`/`& 63` register mask and
/// the same mod-width behavior x86-64/arm64 give immediate shifts
/// (`kernel/bpf/core.c` `SHT`).
#[test]
fn alu32_shifts_follow_the_32_bit_width() {
    // r1 = 0x1_0000_0002 ; r1 <<= 1 (32-bit) -> 4 ; r0 = r1
    let prog = slots(&[
        slot(0x18, 1, 0, 0, 2),
        slot(0x00, 0, 0, 0, 1),
        slot(0x64, 1, 0, 0, 1),             // lsh32 r1, 1
        slot(OP_ALU32_MOV_REG, 0, 1, 0, 0), // mov32 r0, r1
        slot(0x95, 0, 0, 0, 0),
    ]);
    let mut vm = Vm::new();
    assert_eq!(vm.run(&prog).unwrap(), 4);

    // r0 = 1 ; r0 <<= 32 (32-bit) -> count masked to 0 -> 1
    let prog = slots(&[
        slot(OP_ALU32_MOV_IMM, 0, 0, 0, 1),
        slot(0x64, 0, 0, 0, 32),
        slot(0x95, 0, 0, 0, 0),
    ]);
    let mut vm = Vm::new();
    assert_eq!(vm.run(&prog).unwrap(), 1);
}

/// Division and modulo by zero follow the kernel: division yields zero and
/// modulo yields the dividend.
#[test]
fn alu32_division_by_zero_matches_the_kernel() {
    let prog = slots(&[
        slot(OP_ALU32_MOV_IMM, 0, 0, 0, 7),
        slot(OP_DIV32_IMM, 0, 0, 0, 0), // div32 r0, 0
        slot(0x95, 0, 0, 0, 0),
    ]);
    let mut vm = Vm::new();
    assert_eq!(vm.run(&prog).unwrap(), 0);

    let prog = slots(&[
        slot(OP_ALU32_MOV_IMM, 0, 0, 0, 7),
        slot(OP_MOD32_IMM, 0, 0, 0, 0), // mod32 r0, 0
        slot(0x95, 0, 0, 0, 0),
    ]);
    let mut vm = Vm::new();
    assert_eq!(vm.run(&prog).unwrap(), 7);
}

/// `BPF_END` converts to the requested byte order for 16/32/64 bits.
#[test]
fn end_swaps_bytes_to_the_requested_order() {
    // r0 = 0x0102_0304 ; end be32 (0xdc, source bit selects TO_BE) -> swap
    let prog = slots(&[
        slot(OP_ALU32_MOV_IMM, 0, 0, 0, 0x0102_0304u32 as i32),
        slot(0xdc, 0, 0, 0, 32),
        slot(0x95, 0, 0, 0, 0),
    ]);
    let mut vm = Vm::new();
    assert_eq!(vm.run(&prog).unwrap(), 0x0403_0201);

    // r0 = 0x1_2300_0102 ; end le32 (0xd4) truncates to the low 32 bits
    // (the ALU-class TO_LE is the host-order read of `imm` width).
    let prog = slots(&[
        slot(0x18, 0, 0, 0, 0x2300_0102u32 as i32),
        slot(0x00, 0, 0, 0, 1),
        slot(0xd4, 0, 0, 0, 32),
        slot(0x95, 0, 0, 0, 0),
    ]);
    let mut vm = Vm::new();
    assert_eq!(vm.run(&prog).unwrap(), 0x2300_0102);

    // end le16 (0xd4, imm 16) truncates to 16 bits; end be16 (0xdc, imm 16)
    // swaps the low 16 and zero-extends.
    let prog = slots(&[
        slot(OP_ALU32_MOV_IMM, 0, 0, 0, 0x1234),
        slot(0xd4, 0, 0, 0, 16),
        slot(0x95, 0, 0, 0, 0),
    ]);
    let mut vm = Vm::new();
    assert_eq!(vm.run(&prog).unwrap(), 0x1234);

    // le16 on a register with dirty bits above the conversion width: the
    // result is zero-extended, never the whole-register identity.
    let prog = slots(&[
        slot(0x18, 0, 0, 0, 0x2300_0102u32 as i32),
        slot(0x00, 0, 0, 0, 1),
        slot(0xd4, 0, 0, 0, 16),
        slot(0x95, 0, 0, 0, 0),
    ]);
    let mut vm = Vm::new();
    assert_eq!(vm.run(&prog).unwrap(), 0x0102);

    let prog = slots(&[
        slot(OP_ALU32_MOV_IMM, 0, 0, 0, 0x1234),
        slot(0xdc, 0, 0, 0, 16),
        slot(0x95, 0, 0, 0, 0),
    ]);
    let mut vm = Vm::new();
    assert_eq!(vm.run(&prog).unwrap(), 0x3412);

    // end64 le (0xd7) is the BPF_BSWAP encoding: swap within `imm` width.
    let prog = slots(&[
        slot(0x18, 0, 0, 0, 0x0102_0304_0506_0708u64 as i64 as i32),
        slot(0x00, 0, 0, 0, (0x0102_0304_0506_0708u64 >> 32) as i32),
        slot(0xd7, 0, 0, 0, 64),
        slot(0x95, 0, 0, 0, 0),
    ]);
    let mut vm = Vm::new();
    assert_eq!(vm.run(&prog).unwrap(), 0x0807_0605_0403_0201);

    let prog = slots(&[
        slot(0x18, 0, 0, 0, 0x1234),
        slot(0x00, 0, 0, 0, 0),
        slot(0xd7, 0, 0, 0, 16),
        slot(0x95, 0, 0, 0, 0),
    ]);
    let mut vm = Vm::new();
    assert_eq!(vm.run(&prog).unwrap(), 0x3412);

    // The ALU-class encodings accept `imm == 64` as well
    // (`check_alu_op`: imm is 16/32/64; only ALU64 requires TO_LE). The
    // interpreter has no width-64 case there, so both spellings are the
    // identity on the full register.
    let prog = slots(&[
        slot(0x18, 0, 0, 0, 0x0506_0708u32 as i32),
        slot(0x00, 0, 0, 0, 0x0102_0304i32),
        slot(0xd4, 0, 0, 0, 64),
        slot(0x95, 0, 0, 0, 0),
    ]);
    let mut vm = Vm::new();
    assert_eq!(vm.run(&prog).unwrap(), 0x0102_0304_0506_0708);

    let prog = slots(&[
        slot(0x18, 0, 0, 0, 0x0506_0708u32 as i32),
        slot(0x00, 0, 0, 0, 0x0102_0304i32),
        slot(0xdc, 0, 0, 0, 64),
        slot(0x95, 0, 0, 0, 0),
    ]);
    let mut vm = Vm::new();
    assert_eq!(vm.run(&prog).unwrap(), 0x0102_0304_0506_0708);

    // The ALU64 TO_BE encoding (0xdf) has no interpreter label.
    let prog = slots(&[
        slot(0xb7, 0, 0, 0, 1),
        slot(0xdf, 0, 0, 0, 64),
        slot(0x95, 0, 0, 0, 0),
    ]);
    let mut vm = Vm::new();
    assert_eq!(
        vm.run(&prog),
        Err(kbpf::Error::UnknownOpcode(0xdf)),
        "ALU64|END|TO_BE stays unimplemented, like the reference interpreter"
    );
}

/// JMP32 compares only the low 32 bits of each operand, unlike JMP.
#[test]
fn jmp32_compares_the_low_word_only() {
    // r1 = 0x1_0000_0000 ; if r1 == 0 (jmp32) -> r0 = 1
    let prog = slots(&[
        slot(0x18, 1, 0, 0, 0), // lddw r1 = 0x00000001_00000000
        slot(0x00, 0, 0, 0, 1),
        slot(OP_JEQ32_IMM, 1, 0, 2, 0), // jmp32 == 0 -> +2
        slot(OP_ALU32_MOV_IMM, 0, 0, 0, 0),
        slot(OP_JA, 0, 0, 1, 0),
        slot(OP_ALU32_MOV_IMM, 0, 0, 0, 1),
        slot(0x95, 0, 0, 0, 0),
    ]);
    let mut vm = Vm::new();
    assert_eq!(vm.run(&prog).unwrap(), 1);
}

/// Signed 64-bit jumps compare signatures: -1 is not greater than 1, while
/// the unsigned JLT does not treat -1 as small either.
#[test]
fn signed_and_unsigned_jumps_disagree_on_negative_values() {
    // r0 = -1 ; if r0 s> 1 -> r0 = 2 ; else r0 = 0
    let prog = slots(&[
        slot(0xb7, 0, 0, 0, -1), // mov64 r0, -1
        slot(OP_JSGT_IMM, 0, 0, 2, 1),
        slot(OP_ALU32_MOV_IMM, 0, 0, 0, 0),
        slot(0x95, 0, 0, 0, 0),
        slot(0xb7, 0, 0, 0, 2),
        slot(0x95, 0, 0, 0, 0),
    ]);
    let mut vm = Vm::new();
    assert_eq!(vm.run(&prog).unwrap(), 0, "-1 is not signed-greater than 1");

    // r0 = u64::MAX ; if r0 u< 1 -> r0 = 2 ; else r0 = 0
    let prog = slots(&[
        slot(0xb7, 0, 0, 0, -1), // mov64 r0, u64::MAX
        slot(OP_JLT_IMM, 0, 0, 2, 1),
        slot(OP_ALU32_MOV_IMM, 0, 0, 0, 0),
        slot(0x95, 0, 0, 0, 0),
        slot(0xb7, 0, 0, 0, 2),
        slot(0x95, 0, 0, 0, 0),
    ]);
    let mut vm = Vm::new();
    assert_eq!(
        vm.run(&prog).unwrap(),
        0,
        "u64::MAX is not unsigned-less than 1"
    );
}

/// JSET jumps on any bit overlap, not on the mask being fully covered:
/// the device filter's `access_type & mask` check depends on that.
#[test]
fn jset_jumps_on_any_bit_overlap() {
    const OP_JSET_IMM: u8 = 0x45; // BPF_JMP | BPF_JSET | BPF_K

    // r0 = 0b0101 ; if r0 & 0b0100 -> r0 = 1 : the shared bit (2) jumps.
    let prog = slots(&[
        slot(0xb7, 0, 0, 0, 0b0101),
        slot(OP_JSET_IMM, 0, 0, 2, 0b0100),
        slot(0xb7, 0, 0, 0, 0),
        slot(0x95, 0, 0, 0, 0),
        slot(0xb7, 0, 0, 0, 1),
        slot(0x95, 0, 0, 0, 0),
    ]);
    let mut vm = Vm::new();
    assert_eq!(vm.run(&prog).unwrap(), 1, "any shared bit takes the jump");

    // r0 = 0b0101 ; if r0 & 0b1010 -> r0 = 1 : 0b1010 has 0b010 set which
    // r0 lacks, and the old "superset" reading would not jump — Linux does
    // not jump either because no bit overlaps.
    let prog = slots(&[
        slot(0xb7, 0, 0, 0, 0b0101),
        slot(OP_JSET_IMM, 0, 0, 2, 0b1010),
        slot(0xb7, 0, 0, 0, 0),
        slot(0x95, 0, 0, 0, 0),
        slot(0xb7, 0, 0, 0, 1),
        slot(0x95, 0, 0, 0, 0),
    ]);
    let mut vm = Vm::new();
    assert_eq!(vm.run(&prog).unwrap(), 0, "no overlap does not jump");

    // Partial overlap: mask covers a set bit and a clear one.
    let prog = slots(&[
        slot(0xb7, 0, 0, 0, 0b0101),
        slot(OP_JSET_IMM, 0, 0, 2, 0b0110),
        slot(0xb7, 0, 0, 0, 0),
        slot(0x95, 0, 0, 0, 0),
        slot(0xb7, 0, 0, 0, 1),
        slot(0x95, 0, 0, 0, 0),
    ]);
    let mut vm = Vm::new();
    assert_eq!(
        vm.run(&prog).unwrap(),
        1,
        "partial overlap still jumps: any shared bit is enough"
    );
}

/// JMP32's signed conditions compare the low words as `s32`: a negative low
/// word is small, not the huge positive number its zero-extended view gives.
#[test]
fn jmp32_signed_conditions_sign_extend_the_operands() {
    // r1 = 0x1_0000_0000 | 0xFFFF_FFF0 (low word -16); if r1 s< 0 (jmp32)
    // the zero-extended reading would compare 0xFFFF_FFF0 (positive) and
    // never jump — the `s32` reading does.
    let prog = slots(&[
        slot(0x18, 1, 0, 0, 0xFFFF_FFF0u32 as i32), // lddw r1 low word
        slot(0x00, 0, 0, 0, 1),                     // high word 1
        slot(0xc6, 1, 0, 2, 0),                     // jmp32 jslt r1, 0 -> +2
        slot(OP_ALU32_MOV_IMM, 0, 0, 0, 0),
        slot(OP_JA, 0, 0, 1, 0),
        slot(OP_ALU32_MOV_IMM, 0, 0, 0, 1),
        slot(0x95, 0, 0, 0, 0),
    ]);
    let mut vm = Vm::new();
    assert_eq!(
        vm.run(&prog).unwrap(),
        1,
        "a negative s32 low word is signed-less than 0"
    );

    // The 64-bit class reads the full register: 0x1_0000_0000 is positive,
    // so the same condition must NOT jump there.
    let prog = slots(&[
        slot(0x18, 1, 0, 0, 0xFFFF_FFF0u32 as i32),
        slot(0x00, 0, 0, 0, 1),
        slot(0xcd, 1, 0, 2, 0), // jmp jslt r1, 0 (64-bit) -> +2
        slot(OP_ALU32_MOV_IMM, 0, 0, 0, 0),
        slot(OP_JA, 0, 0, 1, 0),
        slot(OP_ALU32_MOV_IMM, 0, 0, 0, 1),
        slot(0x95, 0, 0, 0, 0),
    ]);
    let mut vm = Vm::new();
    assert_eq!(
        vm.run(&prog).unwrap(),
        0,
        "the full 64-bit value is positive and does not jump"
    );
}

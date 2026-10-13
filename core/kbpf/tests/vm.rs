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

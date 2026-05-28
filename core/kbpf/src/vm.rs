// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Minimal eBPF interpreter.
//!
//! At this stage the following opcodes are recognized:
//!
//! - `MOV64_IMM` (`0xb7`): `dst = sign_extend(imm)`
//! - `MOV64_REG` (`0xbf`): `dst = src`
//! - `ADD64_IMM` (`0x07`): `dst = dst.wrapping_add(sign_extend(imm))`
//! - `ADD64_REG` (`0x0f`): `dst = dst.wrapping_add(src)`
//! - `JA`        (`0x05`): unconditional relative jump by `off` slots
//! - `JEQ_IMM`   (`0x15`): jump by `off` slots if `dst == sign_extend(imm)`
//! - `EXIT`     (`0x95`): stop and return `r0`
//! - `CALL`     (`0x85`): dispatch to a helper registered via
//!   [`Vm::register_helper`]; arguments come from `r1..=r5`, the
//!   return value lands in `r0`.
//!
//! All ALU operations follow eBPF's two's-complement wrapping semantics:
//! overflow is well-defined and never traps. Branch targets are computed
//! as `next_pc + off`, where `next_pc` is the slot index of the
//! instruction that immediately follows the branch in linear order.
//!
//! Programs always start with a clean register file; if the host needs
//! to pass a context pointer or other arguments to the program (the
//! traditional eBPF "ctx in `r1`" entry convention), it can seed
//! `r1..=r5` via [`Vm::run_with_initial_regs`]. [`Vm::run`] is the same
//! call with all initial registers set to zero.
//!
//! To keep buggy programs from looping forever, [`Vm::run`] enforces a
//! per-run instruction count, configurable via
//! [`Vm::with_instruction_limit`].

use crate::{
    error::{Error, Result},
    insn::{Insn, SLOT_SIZE},
};

/// `mov64 dst, imm`  (BPF_ALU64 | BPF_MOV | BPF_K).
const OP_MOV64_IMM: u8 = 0xb7;
/// `mov64 dst, src`  (BPF_ALU64 | BPF_MOV | BPF_X).
const OP_MOV64_REG: u8 = 0xbf;
/// `add64 dst, imm`  (BPF_ALU64 | BPF_ADD | BPF_K).
const OP_ADD64_IMM: u8 = 0x07;
/// `add64 dst, src`  (BPF_ALU64 | BPF_ADD | BPF_X).
const OP_ADD64_REG: u8 = 0x0f;
/// `ja off`           (BPF_JMP   | BPF_JA).
const OP_JA: u8 = 0x05;
/// `jeq dst, imm, off` (BPF_JMP  | BPF_JEQ | BPF_K).
const OP_JEQ_IMM: u8 = 0x15;
/// `exit`             (BPF_JMP   | BPF_EXIT).
const OP_EXIT: u8 = 0x95;
/// `call imm`         (BPF_JMP   | BPF_CALL).
const OP_CALL: u8 = 0x85;

/// Number of general-purpose eBPF registers (`r0..=r10`).
pub const NUM_REGS: usize = 11;

/// Default per-run instruction budget.
///
/// Mirrors the order of magnitude of the historical Linux limit; the
/// exact value here is just a defensive upper bound for our interpreter
/// and can be lowered in tests via [`Vm::with_instruction_limit`].
const DEFAULT_INSTRUCTION_LIMIT: usize = 1_000_000;

/// Maximum number of helper functions a [`Vm`] can hold.
///
/// Sized to comfortably cover the basic helper set we plan to expose
/// from `host.rs` while keeping the table small enough to live inline
/// in [`Vm`] (no allocator, no `BTreeMap`).
pub const HELPER_TABLE_CAPACITY: usize = 32;

/// Signature of a BPF helper function.
///
/// Matches the Linux eBPF helper ABI: up to five 64-bit arguments
/// arriving in `r1..=r5`, a single 64-bit return value going back into
/// `r0`. Helpers that need fewer arguments simply ignore the trailing
/// ones.
pub type HelperFn = fn(u64, u64, u64, u64, u64) -> u64;

#[derive(Debug, Clone, Copy)]
struct HelperEntry {
    id: u32,
    func: HelperFn,
}

/// Initial values for the argument registers `r1..=r5`.
///
/// Linux eBPF programs receive their context pointer in `r1` on entry;
/// when chained helpers want extra arguments, they live in `r2..=r5`.
/// `r0` (return value) and `r6..=r10` (callee-saved / frame pointer)
/// always start at zero.
#[derive(Debug, Default, Clone, Copy)]
pub struct InitialRegs {
    /// Value placed in `r1` before the program starts (typically the
    /// "context" pointer).
    pub r1: u64,
    /// Value placed in `r2` before the program starts.
    pub r2: u64,
    /// Value placed in `r3` before the program starts.
    pub r3: u64,
    /// Value placed in `r4` before the program starts.
    pub r4: u64,
    /// Value placed in `r5` before the program starts.
    pub r5: u64,
}

/// Stripped-down BPF interpreter state.
///
/// Holds the register file, a per-run instruction limit, and a small
/// inline helper table. A stack and longer-lived program state will be
/// added as more instructions are introduced.
pub struct Vm {
    regs: [u64; NUM_REGS],
    instruction_limit: usize,
    helpers: [Option<HelperEntry>; HELPER_TABLE_CAPACITY],
}

impl Default for Vm {
    fn default() -> Self {
        Self::new()
    }
}

impl Vm {
    /// Create a fresh VM with all registers zeroed, no helpers
    /// registered, and the default instruction limit applied.
    pub const fn new() -> Self {
        Self {
            regs: [0; NUM_REGS],
            instruction_limit: DEFAULT_INSTRUCTION_LIMIT,
            helpers: [None; HELPER_TABLE_CAPACITY],
        }
    }

    /// Override the per-run instruction budget. Useful primarily for
    /// tests that want a tight bound on infinite-loop detection.
    pub const fn with_instruction_limit(mut self, limit: usize) -> Self {
        self.instruction_limit = limit;
        self
    }

    /// Bind a helper function to `id` so that programs running on this
    /// VM can invoke it via `call id`.
    ///
    /// If `id` is already registered, the existing entry is replaced
    /// (this never fails). Otherwise a free slot is taken; if the table
    /// is at [`HELPER_TABLE_CAPACITY`], [`Error::HelperTableFull`] is
    /// returned and no change is made.
    pub fn register_helper(&mut self, id: u32, func: HelperFn) -> Result<()> {
        for slot in self.helpers.iter_mut() {
            if let Some(entry) = slot
                && entry.id == id
            {
                entry.func = func;
                return Ok(());
            }
        }
        for slot in self.helpers.iter_mut() {
            if slot.is_none() {
                *slot = Some(HelperEntry { id, func });
                return Ok(());
            }
        }
        Err(Error::HelperTableFull)
    }

    fn lookup_helper(&self, id: u32) -> Result<HelperFn> {
        for slot in self.helpers.iter() {
            if let Some(entry) = slot
                && entry.id == id
            {
                return Ok(entry.func);
            }
        }
        Err(Error::UnknownHelper(id))
    }

    /// Execute `prog` with the argument registers seeded from
    /// `initial`, and return the value of `r0` produced by the
    /// terminating `exit` instruction.
    ///
    /// All registers are reset before the program starts: `r1..=r5`
    /// take their values from `initial` and the rest are zeroed.
    ///
    /// `prog` must be a flat sequence of 8-byte instruction slots.
    pub fn run_with_initial_regs(&mut self, prog: &[u8], initial: InitialRegs) -> Result<u64> {
        if !prog.len().is_multiple_of(SLOT_SIZE) {
            return Err(Error::UnalignedProgram);
        }
        let num_slots = prog.len() / SLOT_SIZE;

        self.regs = [0; NUM_REGS];
        self.regs[1] = initial.r1;
        self.regs[2] = initial.r2;
        self.regs[3] = initial.r3;
        self.regs[4] = initial.r4;
        self.regs[5] = initial.r5;

        let mut pc: usize = 0;
        let mut steps: usize = 0;

        while pc < num_slots {
            if steps >= self.instruction_limit {
                return Err(Error::InstructionLimitExceeded);
            }
            steps += 1;

            let off_byte = pc * SLOT_SIZE;
            let mut slot = [0u8; SLOT_SIZE];
            slot.copy_from_slice(&prog[off_byte..off_byte + SLOT_SIZE]);
            let insn = Insn::from_bytes(slot);
            pc += 1;

            match insn.opc {
                OP_MOV64_IMM => {
                    let dst = reg_index(insn.dst)?;
                    self.regs[dst] = insn.imm as i64 as u64;
                }
                OP_MOV64_REG => {
                    let dst = reg_index(insn.dst)?;
                    let src = reg_index(insn.src)?;
                    self.regs[dst] = self.regs[src];
                }
                OP_ADD64_IMM => {
                    let dst = reg_index(insn.dst)?;
                    self.regs[dst] = self.regs[dst].wrapping_add(insn.imm as i64 as u64);
                }
                OP_ADD64_REG => {
                    let dst = reg_index(insn.dst)?;
                    let src = reg_index(insn.src)?;
                    self.regs[dst] = self.regs[dst].wrapping_add(self.regs[src]);
                }
                OP_JA => {
                    pc = jump_target(pc, insn.off, num_slots)?;
                }
                OP_JEQ_IMM => {
                    let dst = reg_index(insn.dst)?;
                    if self.regs[dst] == insn.imm as i64 as u64 {
                        pc = jump_target(pc, insn.off, num_slots)?;
                    }
                }
                OP_CALL => {
                    let id = insn.imm as u32;
                    let func = self.lookup_helper(id)?;
                    self.regs[0] = func(
                        self.regs[1],
                        self.regs[2],
                        self.regs[3],
                        self.regs[4],
                        self.regs[5],
                    );
                }
                OP_EXIT => {
                    return Ok(self.regs[0]);
                }
                opc => return Err(Error::UnknownOpcode(opc)),
            }
        }

        Err(Error::EndOfProgram)
    }

    /// Execute `prog` with all registers initialised to zero.
    ///
    /// Equivalent to `run_with_initial_regs(prog, InitialRegs::default())`.
    pub fn run(&mut self, prog: &[u8]) -> Result<u64> {
        self.run_with_initial_regs(prog, InitialRegs::default())
    }
}

/// Validate a 4-bit register field against [`NUM_REGS`].
fn reg_index(reg: u8) -> Result<usize> {
    let idx = reg as usize;
    if idx >= NUM_REGS {
        return Err(Error::InvalidRegister(reg));
    }
    Ok(idx)
}

/// Compute the target slot of a relative jump.
///
/// `pc` is the slot index of the instruction that follows the branch
/// (the BPF convention) and `off` is the signed slot delta encoded in
/// the branch instruction. The target must land on a slot within the
/// program; `num_slots` is the program's length in slots.
fn jump_target(pc: usize, off: i16, num_slots: usize) -> Result<usize> {
    let target = (pc as i64).wrapping_add(off as i64);
    if target < 0 || target >= num_slots as i64 {
        return Err(Error::JumpOutOfBounds);
    }
    Ok(target as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}

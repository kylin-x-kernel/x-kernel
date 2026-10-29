// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Minimal eBPF interpreter.
//!
//! At this stage the following opcodes are recognized:
//!
//! Load / store:
//!
//! - `LD_DW_IMM` (`0x18`): `dst = imm64`
//! - `LDX_MEM_{B,H,W,DW}`: load from stack or read-only map memory into `dst`
//! - `ST_MEM_{B,H,W,DW}`: store immediate into `[dst+off]`
//! - `STX_MEM_{B,H,W,DW}`: store register into `[dst+off]`
//!
//! ALU64:
//!
//! - `ADD64_IMM` (`0x07`): `dst = dst.wrapping_add(sign_extend(imm))`
//! - `ADD64_REG` (`0x0f`): `dst = dst.wrapping_add(src)`
//! - `MOV64_IMM` (`0xb7`): `dst = sign_extend(imm)`
//! - `MOV64_REG` (`0xbf`): `dst = src`
//!
//! Control flow:
//!
//! - `JA`      (`0x05`): unconditional relative jump by `off` slots
//! - `JEQ_IMM` (`0x15`): jump by `off` slots if `dst == sign_extend(imm)`
//! - `CALL`    (`0x85`): dispatch to a helper; arguments in `r1..=r5`,
//!   return value in `r0`
//! - `EXIT`    (`0x95`): stop and return `r0`
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

use alloc::{sync::Arc, vec::Vec};

use crate::{
    error::{Error, Result},
    helpers::BPF_FUNC_TRACE_PRINTK,
    insn::{Insn, SLOT_SIZE},
};

// BPF_LD
/// `lddw dst, imm64` (`BPF_LD | BPF_DW | BPF_IMM`).
const OP_LD_DW_IMM: u8 = 0x18;
/// `lddw` source marker for map-value addresses.
const BPF_PSEUDO_MAP_VALUE: u8 = 2;

// eBPF opcode fields: class (bits 0-2), source (bit 3), operation (bits 4-7).
const OP_CALL: u8 = 0x85;
const OP_EXIT: u8 = 0x95;
const CLASS_ALU: u8 = 0x04;
const CLASS_JMP: u8 = 0x05;
const CLASS_JMP32: u8 = 0x06;
const CLASS_ALU64: u8 = 0x07;
const SOURCE_REG: u8 = 0x08;

// ALU operations (bits 4-7).
const ALU_ADD: u8 = 0x00;
const ALU_SUB: u8 = 0x10;
const ALU_MUL: u8 = 0x20;
const ALU_DIV: u8 = 0x30;
const ALU_OR: u8 = 0x40;
const ALU_AND: u8 = 0x50;
const ALU_LSH: u8 = 0x60;
const ALU_RSH: u8 = 0x70;
const ALU_NEG: u8 = 0x80;
const ALU_MOD: u8 = 0x90;
const ALU_XOR: u8 = 0xa0;
const ALU_MOV: u8 = 0xb0;
const ALU_ARSH: u8 = 0xc0;
const ALU_END: u8 = 0xd0;

// Jump conditions (bits 4-7). The unsigned variants compare the operands as
// unsigned; the `S` variants compare them as signed.
const JMP_JA: u8 = 0x00;
const JMP_JEQ: u8 = 0x10;
const JMP_JGT: u8 = 0x20;
const JMP_JGE: u8 = 0x30;
const JMP_JSET: u8 = 0x40;
const JMP_JNE: u8 = 0x50;
const JMP_JSGT: u8 = 0x60;
const JMP_JSGE: u8 = 0x70;
const JMP_JLT: u8 = 0xa0;
const JMP_JLE: u8 = 0xb0;
const JMP_JSLT: u8 = 0xc0;
const JMP_JSLE: u8 = 0xd0;

// BPF_LDX | BPF_MEM
/// `ldxb dst, [src+off]`.
const OP_LDX_MEM_B: u8 = 0x71;
/// `ldxh dst, [src+off]`.
const OP_LDX_MEM_H: u8 = 0x69;
/// `ldxw dst, [src+off]`.
const OP_LDX_MEM_W: u8 = 0x61;
/// `ldxdw dst, [src+off]`.
const OP_LDX_MEM_DW: u8 = 0x79;

// BPF_ST | BPF_MEM
/// `stb [dst+off], imm`.
const OP_ST_MEM_B: u8 = 0x72;
/// `sth [dst+off], imm`.
const OP_ST_MEM_H: u8 = 0x6a;
/// `stw [dst+off], imm`.
const OP_ST_MEM_W: u8 = 0x62;
/// `stdw [dst+off], imm`.
const OP_ST_MEM_DW: u8 = 0x7a;

// BPF_STX | BPF_MEM
/// `stxb [dst+off], src`.
const OP_STX_MEM_B: u8 = 0x73;
/// `stxh [dst+off], src`.
const OP_STX_MEM_H: u8 = 0x6b;
/// `stxw [dst+off], src`.
const OP_STX_MEM_W: u8 = 0x63;
/// `stxdw [dst+off], src`.
const OP_STX_MEM_DW: u8 = 0x7b;

// BPF_ALU64

// BPF_JMP

/// Number of general-purpose eBPF registers (`r0..=r10`).
pub const NUM_REGS: usize = 11;
/// Linux eBPF stack size in bytes.
pub const STACK_SIZE: usize = 512;

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

/// Immutable memory owned by the host and visible to a BPF program.
#[derive(Debug, Clone)]
pub struct ReadOnlyMemory {
    id: u32,
    base: u64,
    bytes: Arc<[u8]>,
}

impl ReadOnlyMemory {
    /// Exposes `bytes` as a map value referenced by `BPF_PSEUDO_MAP_VALUE`.
    pub fn map_value(id: u32, bytes: Arc<[u8]>) -> Self {
        Self {
            id,
            base: bytes.as_ptr() as u64,
            bytes,
        }
    }

    /// Exposes `bytes` as the program's context argument, returning the
    /// memory and the base address the context pointer (`r1`) must carry.
    ///
    /// This is how a host hands a `struct ..._ctx` to a program: the bytes
    /// become read-only program memory and `r1` points at their start.
    pub fn context(bytes: Arc<[u8]>) -> (Self, u64) {
        // `u32::MAX` is reserved for the context so an `lddw` map-value
        // relocation keyed by a caller-chosen id (map fd numbers) can never
        // alias the context region.
        let base = bytes.as_ptr() as u64;
        (
            Self {
                id: u32::MAX,
                base,
                bytes,
            },
            base,
        )
    }

    fn addr_at_offset(&self, offset: u32) -> Result<u64> {
        if offset as usize > self.bytes.len() {
            return Err(Error::MemoryOutOfBounds);
        }
        self.base
            .checked_add(offset as u64)
            .ok_or(Error::MemoryOutOfBounds)
    }

    fn slice(&self, addr: u64, len: usize) -> Option<&[u8]> {
        let end = addr.checked_add(len as u64)?;
        let region_end = self.base.checked_add(self.bytes.len() as u64)?;
        if addr < self.base || end > region_end {
            return None;
        }

        let start = (addr - self.base) as usize;
        let end = (end - self.base) as usize;
        Some(&self.bytes[start..end])
    }
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
    stack: [u8; STACK_SIZE],
    instruction_limit: usize,
    helpers: [Option<HelperEntry>; HELPER_TABLE_CAPACITY],
    read_only: Vec<ReadOnlyMemory>,
}

impl Default for Vm {
    fn default() -> Self {
        Self::new()
    }
}

impl Vm {
    /// Create a fresh VM with all registers zeroed and the default
    /// instruction limit applied.
    pub const fn new() -> Self {
        Self {
            regs: [0; NUM_REGS],
            stack: [0; STACK_SIZE],
            instruction_limit: DEFAULT_INSTRUCTION_LIMIT,
            helpers: [None; HELPER_TABLE_CAPACITY],
            read_only: Vec::new(),
        }
    }

    /// Override the per-run instruction budget. Useful primarily for
    /// tests that want a tight bound on infinite-loop detection.
    pub const fn with_instruction_limit(mut self, limit: usize) -> Self {
        self.instruction_limit = limit;
        self
    }

    /// Adds immutable memory to the default execution environment.
    pub fn add_read_only_memory(&mut self, memory: ReadOnlyMemory) {
        self.read_only.push(memory);
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
        self.stack = [0; STACK_SIZE];
        self.regs[1] = initial.r1;
        self.regs[2] = initial.r2;
        self.regs[3] = initial.r3;
        self.regs[4] = initial.r4;
        self.regs[5] = initial.r5;
        self.regs[10] = self.stack.as_mut_ptr() as u64 + STACK_SIZE as u64;

        let mut pc: usize = 0;
        let mut steps: usize = 0;

        while pc < num_slots {
            if steps >= self.instruction_limit {
                return Err(Error::InstructionLimitExceeded);
            }
            steps += 1;

            let insn = read_insn(prog, pc);
            pc += 1;

            match insn.opc {
                // BPF_LD
                OP_LD_DW_IMM => {
                    let dst = reg_index(insn.dst)?;
                    if pc >= num_slots {
                        return Err(Error::IncompleteWideImmediate);
                    }
                    let next = read_insn(prog, pc);
                    pc += 1;
                    self.regs[dst] = if insn.src == BPF_PSEUDO_MAP_VALUE {
                        self.read_only_memory_addr(insn.imm as u32, next.imm as u32)?
                    } else {
                        let low = insn.imm as u32 as u64;
                        let high = (next.imm as u32 as u64) << 32;
                        high | low
                    };
                }
                // BPF_LDX | BPF_MEM
                OP_LDX_MEM_B | OP_LDX_MEM_H | OP_LDX_MEM_W | OP_LDX_MEM_DW => {
                    let dst = reg_index(insn.dst)?;
                    let src = reg_index(insn.src)?;
                    let width = mem_width(insn.opc)?;
                    let addr = self.regs[src].wrapping_add(insn.off as i64 as u64);
                    self.regs[dst] = self.read_reg_value(addr, width)?;
                }
                // BPF_ST | BPF_MEM
                OP_ST_MEM_B | OP_ST_MEM_H | OP_ST_MEM_W | OP_ST_MEM_DW => {
                    let dst = reg_index(insn.dst)?;
                    let width = mem_width(insn.opc)?;
                    let addr = self.regs[dst].wrapping_add(insn.off as i64 as u64);
                    self.write_stack_value(addr, width, insn.imm as i64 as u64)?;
                }
                // BPF_STX | BPF_MEM
                OP_STX_MEM_B | OP_STX_MEM_H | OP_STX_MEM_W | OP_STX_MEM_DW => {
                    let dst = reg_index(insn.dst)?;
                    let src = reg_index(insn.src)?;
                    let width = mem_width(insn.opc)?;
                    let addr = self.regs[dst].wrapping_add(insn.off as i64 as u64);
                    self.write_stack_value(addr, width, self.regs[src])?;
                }
                // BPF_ALU64
                // BPF_ALU64 — the 64-bit ALU class.
                opc if opc & 0x07 == CLASS_ALU64 => self.execute_alu(insn, true)?,
                // BPF_ALU — the 32-bit ALU class; results zero-extend.
                opc if opc & 0x07 == CLASS_ALU => self.execute_alu(insn, false)?,
                // `call` and `exit` live in the jump class but terminate or
                // dispatch instead of branching.
                OP_CALL => {
                    self.regs[0] = self.call_helper(insn.imm as u32)?;
                }
                OP_EXIT => {
                    return Ok(self.regs[0]);
                }
                // BPF_JMP — 64-bit jump conditions.
                opc if opc & 0x07 == CLASS_JMP => {
                    pc = self.execute_jump(insn, pc, num_slots, false)?;
                }
                // BPF_JMP32 — jump conditions on the low 32 bits only.
                opc if opc & 0x07 == CLASS_JMP32 => {
                    pc = self.execute_jump(insn, pc, num_slots, true)?;
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

    /// Executes one instruction of the ALU or ALU64 class.
    ///
    /// 64-bit operations work on the full register; 32-bit operations work on
    /// the low 32 bits and zero-extend their result, per the eBPF
    /// specification.
    fn execute_alu(&mut self, insn: Insn, wide: bool) -> Result<()> {
        let dst = reg_index(insn.dst)?;
        let use_register = insn.opc & SOURCE_REG == SOURCE_REG;
        let operation = insn.opc & 0xf0;

        if operation == ALU_MOV {
            self.regs[dst] = if wide {
                if use_register {
                    self.regs[reg_index(insn.src)?]
                } else {
                    insn.imm as i64 as u64
                }
            } else if use_register {
                self.regs[reg_index(insn.src)?] as u32 as u64
            } else {
                insn.imm as u32 as u64
            };
            return Ok(());
        }
        if operation == ALU_END {
            // The 6.8 interpreter's byte-order semantics on a little-endian
            // host (`kernel/bpf/core.c`): the ALU-class encodings convert
            // host order — TO_BE (0xdc) swaps within `imm` and TO_LE (0xd4)
            // truncates — both zero-extending the result; the ALU64-class
            // TO_LE encoding (0xd7) is the pure `BPF_BSWAP` form (swap
            // within `imm`, zero-extended except width 64). The interpreter's
            // ALU-class switches have no `imm == 64` case, so `end le64`/
            // `end be64` in that class leave the register unchanged — the
            // verifier accepts the encoding (`check_alu_op`: imm is
            // 16/32/64; only the ALU64 class requires TO_LE). The ALU64
            // TO_BE encoding (0xdf) has no interpreter label and
            // `bpf_opcode_in_insntable()` rejects it at load.
            let value = self.regs[dst];
            let converted = match (insn.opc, insn.imm) {
                (0xdc, 16) => (value as u16).swap_bytes() as u64,
                (0xdc, 32) => (value as u32).swap_bytes() as u64,
                (0xdc, 64) => value,
                (0xd4, 16) => value as u16 as u64,
                (0xd4, 32) => value as u32 as u64,
                (0xd4, 64) => value,
                (0xd7, 16) => (value as u16).swap_bytes() as u64,
                (0xd7, 32) => (value as u32).swap_bytes() as u64,
                (0xd7, 64) => value.swap_bytes(),
                _ => return Err(Error::UnknownOpcode(insn.opc)),
            };
            self.regs[dst] = converted;
            return Ok(());
        }

        let operand = if use_register {
            self.regs[reg_index(insn.src)?]
        } else if wide {
            insn.imm as i64 as u64
        } else {
            insn.imm as u32 as u64
        };
        self.regs[dst] = if wide {
            alu64_operation(operation, self.regs[dst], operand, insn.opc)?
        } else {
            alu32_operation(operation, self.regs[dst] as u32, operand as u32, insn.opc)? as u64
        };
        Ok(())
    }

    /// Executes one instruction of the JMP or JMP32 class.
    ///
    /// `wide == false` compares the full 64-bit registers; `wide == true`
    /// (JMP32) compares only the low 32 bits. The branch target is a signed
    /// slot delta from the instruction that follows the branch.
    fn execute_jump(
        &mut self,
        insn: Insn,
        pc: usize,
        num_slots: usize,
        narrow: bool,
    ) -> Result<usize> {
        let operation = insn.opc & 0xf0;
        let use_register = insn.opc & SOURCE_REG == SOURCE_REG;
        let dst = reg_index(insn.dst)?;

        if operation == JMP_JA {
            return jump_target(pc, insn.off, num_slots);
        }

        let (left, right) = if narrow {
            let right = if use_register {
                self.regs[reg_index(insn.src)?] as u32 as u64
            } else {
                insn.imm as u32 as u64
            };
            (self.regs[dst] as u32 as u64, right)
        } else {
            let right = if use_register {
                self.regs[reg_index(insn.src)?]
            } else {
                insn.imm as i64 as u64
            };
            (self.regs[dst], right)
        };

        let taken = match operation {
            JMP_JEQ => left == right,
            JMP_JNE => left != right,
            JMP_JGT => left > right,
            JMP_JGE => left >= right,
            // Linux jumps when the bitwise AND is non-zero.
            JMP_JSET => left & right != 0,
            JMP_JLT => left < right,
            JMP_JLE => left <= right,
            // The narrow (JMP32) class sign-extends both operands to compare
            // them as `s32` — the zero-extended view used above would turn a
            // negative low word into a huge positive number.
            JMP_JSGT if narrow => (left as u32 as i32) as i64 > (right as u32 as i32) as i64,
            JMP_JSGE if narrow => (left as u32 as i32) as i64 >= (right as u32 as i32) as i64,
            JMP_JSLT if narrow => ((left as u32 as i32) as i64) < ((right as u32 as i32) as i64),
            JMP_JSLE if narrow => ((left as u32 as i32) as i64) <= ((right as u32 as i32) as i64),
            JMP_JSGT => (left as i64) > (right as i64),
            JMP_JSGE => (left as i64) >= (right as i64),
            JMP_JSLT => (left as i64) < (right as i64),
            JMP_JSLE => (left as i64) <= (right as i64),
            _ => return Err(Error::UnknownOpcode(insn.opc)),
        };
        if taken {
            jump_target(pc, insn.off, num_slots)
        } else {
            Ok(pc)
        }
    }

    fn call_helper(&self, id: u32) -> Result<u64> {
        if let Ok(func) = self.lookup_helper(id) {
            return Ok(func(
                self.regs[1],
                self.regs[2],
                self.regs[3],
                self.regs[4],
                self.regs[5],
            ));
        }

        match id {
            BPF_FUNC_TRACE_PRINTK => {
                let fmt = self.read_memory(self.regs[1], self.regs[2] as usize)?;
                Ok(crate::helpers::trace_printk(
                    fmt,
                    self.regs[3],
                    self.regs[4],
                    self.regs[5],
                ))
            }
            _ => Err(Error::UnknownHelper(id)),
        }
    }

    fn stack_slice(&self, addr: u64, len: usize) -> Result<&[u8]> {
        let range = self.stack_range(addr, len)?;
        Ok(&self.stack[range])
    }

    fn stack_slice_mut(&mut self, addr: u64, len: usize) -> Result<&mut [u8]> {
        let range = self.stack_range(addr, len)?;
        Ok(&mut self.stack[range])
    }

    /// Read `width` bytes from the BPF stack or bound read-only map memory.
    fn read_memory(&self, addr: u64, len: usize) -> Result<&[u8]> {
        if let Ok(slice) = self.stack_slice(addr, len) {
            return Ok(slice);
        }

        for memory in &self.read_only {
            if let Some(slice) = memory.slice(addr, len) {
                return Ok(slice);
            }
        }

        Err(Error::MemoryOutOfBounds)
    }

    /// Load a zero-extended register value from stack or read-only memory.
    fn read_reg_value(&self, addr: u64, width: usize) -> Result<u64> {
        Ok(load_memory_value(self.read_memory(addr, width)?))
    }

    /// Store a register value into the BPF stack.
    fn write_stack_value(&mut self, addr: u64, width: usize, value: u64) -> Result<()> {
        store_memory_value(self.stack_slice_mut(addr, width)?, value);
        Ok(())
    }

    fn read_only_memory_addr(&self, id: u32, offset: u32) -> Result<u64> {
        for memory in &self.read_only {
            if memory.id == id {
                return memory.addr_at_offset(offset);
            }
        }
        Err(Error::MemoryOutOfBounds)
    }

    fn stack_range(&self, addr: u64, len: usize) -> Result<core::ops::Range<usize>> {
        let stack_start = self.stack.as_ptr() as u64;
        let stack_end = stack_start + STACK_SIZE as u64;
        let end = addr
            .checked_add(len as u64)
            .ok_or(Error::MemoryOutOfBounds)?;

        if addr < stack_start || end > stack_end {
            return Err(Error::MemoryOutOfBounds);
        }

        let start_offset = (addr - stack_start) as usize;
        let end_offset = (end - stack_start) as usize;
        Ok(start_offset..end_offset)
    }
}

fn read_insn(prog: &[u8], slot_idx: usize) -> Insn {
    let off_byte = slot_idx * SLOT_SIZE;
    let mut slot = [0u8; SLOT_SIZE];
    slot.copy_from_slice(&prog[off_byte..off_byte + SLOT_SIZE]);
    Insn::from_bytes(slot)
}

/// Validate a 4-bit register field against [`NUM_REGS`].
fn reg_index(reg: u8) -> Result<usize> {
    let idx = reg as usize;
    if idx >= NUM_REGS {
        return Err(Error::InvalidRegister(reg));
    }
    Ok(idx)
}

/// Map a BPF memory opcode to its access width in bytes.
fn mem_width(opc: u8) -> Result<usize> {
    Ok(match opc {
        OP_LDX_MEM_B | OP_ST_MEM_B | OP_STX_MEM_B => 1,
        OP_LDX_MEM_H | OP_ST_MEM_H | OP_STX_MEM_H => 2,
        OP_LDX_MEM_W | OP_ST_MEM_W | OP_STX_MEM_W => 4,
        OP_LDX_MEM_DW | OP_ST_MEM_DW | OP_STX_MEM_DW => 8,
        opc => return Err(Error::UnknownOpcode(opc)),
    })
}

/// Decode a little-endian memory slice into a zero-extended register value.
fn load_memory_value(bytes: &[u8]) -> u64 {
    let mut value = 0u64;
    for (idx, byte) in bytes.iter().copied().enumerate() {
        value |= (byte as u64) << (idx * 8);
    }
    value
}

/// Encode a register value into a little-endian memory slice.
fn store_memory_value(dst: &mut [u8], value: u64) {
    for (idx, byte) in dst.iter_mut().enumerate() {
        *byte = (value >> (idx * 8)) as u8;
    }
}

/// Applies one ALU/ALU64 arithmetic-logic operation.
///
/// Division and modulo by zero yield zero and the dividend respectively,
/// matching the kernel; shifts wrap by the operand width; the 32-bit results
/// are computed on `u32` and returned for zero-extension.
fn alu64_operation(operation: u8, left: u64, right: u64, opc: u8) -> Result<u64> {
    // The interpreter masks register-source shift counts with 63/31
    // (`SHT` in `kernel/bpf/core.c`) and leaves immediate shifts unmasked;
    // this port uses the masked (wrapping) form for both so an over-wide
    // immediate cannot panic — matching the kernel's documented
    // implementation-defined over-shift behavior.
    Ok(match operation {
        ALU_ADD => left.wrapping_add(right),
        ALU_SUB => left.wrapping_sub(right),
        ALU_MUL => left.wrapping_mul(right),
        ALU_DIV => left.checked_div(right).unwrap_or(0),
        ALU_MOD => left.checked_rem(right).unwrap_or(left),
        ALU_OR => left | right,
        ALU_AND => left & right,
        ALU_XOR => left ^ right,
        ALU_LSH => left.wrapping_shl(right as u32 & 63),
        ALU_RSH => left.wrapping_shr(right as u32 & 63),
        ALU_ARSH => ((left as i64).wrapping_shr(right as u32 & 63)) as u64,
        ALU_NEG => (left as i64).wrapping_neg() as u64,
        _ => return Err(Error::UnknownOpcode(opc)),
    })
}

/// Applies one ALU (32-bit) arithmetic-logic operation; the result is
/// zero-extended by the caller.
fn alu32_operation(operation: u8, left: u32, right: u32, opc: u8) -> Result<u32> {
    Ok(match operation {
        ALU_ADD => left.wrapping_add(right),
        ALU_SUB => left.wrapping_sub(right),
        ALU_MUL => left.wrapping_mul(right),
        ALU_DIV => left.checked_div(right).unwrap_or(0),
        ALU_MOD => left.checked_rem(right).unwrap_or(left),
        ALU_OR => left | right,
        ALU_AND => left & right,
        ALU_XOR => left ^ right,
        ALU_LSH => left.wrapping_shl(right & 31),
        ALU_RSH => left.wrapping_shr(right & 31),
        ALU_ARSH => (left as i32).wrapping_shr(right & 31) as u32,
        ALU_NEG => (left as i32).wrapping_neg() as u32,
        _ => return Err(Error::UnknownOpcode(opc)),
    })
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

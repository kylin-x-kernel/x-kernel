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
/// `add64 dst, imm`  (BPF_ALU64 | BPF_ADD | BPF_K).
const OP_ADD64_IMM: u8 = 0x07;
/// `add64 dst, src`  (BPF_ALU64 | BPF_ADD | BPF_X).
const OP_ADD64_REG: u8 = 0x0f;
/// `mov64 dst, imm`  (BPF_ALU64 | BPF_MOV | BPF_K).
const OP_MOV64_IMM: u8 = 0xb7;
/// `mov64 dst, src`  (BPF_ALU64 | BPF_MOV | BPF_X).
const OP_MOV64_REG: u8 = 0xbf;

// BPF_JMP
/// `ja off`           (BPF_JMP   | BPF_JA).
const OP_JA: u8 = 0x05;
/// `jeq dst, imm, off` (BPF_JMP  | BPF_JEQ | BPF_K).
const OP_JEQ_IMM: u8 = 0x15;
/// `call imm`         (BPF_JMP   | BPF_CALL).
const OP_CALL: u8 = 0x85;
/// `exit`             (BPF_JMP   | BPF_EXIT).
const OP_EXIT: u8 = 0x95;

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
                OP_ADD64_IMM => {
                    let dst = reg_index(insn.dst)?;
                    self.regs[dst] = self.regs[dst].wrapping_add(insn.imm as i64 as u64);
                }
                OP_ADD64_REG => {
                    let dst = reg_index(insn.dst)?;
                    let src = reg_index(insn.src)?;
                    self.regs[dst] = self.regs[dst].wrapping_add(self.regs[src]);
                }
                OP_MOV64_IMM => {
                    let dst = reg_index(insn.dst)?;
                    self.regs[dst] = insn.imm as i64 as u64;
                }
                OP_MOV64_REG => {
                    let dst = reg_index(insn.dst)?;
                    let src = reg_index(insn.src)?;
                    self.regs[dst] = self.regs[src];
                }
                // BPF_JMP
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
                    self.regs[0] = self.call_helper(id)?;
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
        vm.add_read_only_memory(ReadOnlyMemory::map_value(
            MAP_ID,
            alloc::sync::Arc::from(&b"hi\n\0"[..]),
        ));
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
}

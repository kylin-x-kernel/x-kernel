# kcpu — Security and Reliability Analysis

## Cross-architecture CPU identification boundary

`src/cpuinfo.rs` publishes immutable, bounded per-logical-CPU snapshots through
`klazy::Once`. Missing slots return `None` without initialization or waiting;
readers perform no register access or firmware parsing. CPU-local detection runs
before scheduling, while RISC-V firmware publication may identify unstarted APs.
No reset/hotplug lifetime is claimed. The migration-safe intersection is the
one `system_hwcap()` folds from the identified CPUs; it is recomputed on each
call rather than maintained as state, so a CPU that comes online later is
included the next time the word is published.

AArch64/LoongArch register reads use the existing architecture dependency's safe
read interfaces; this adds no local unsafe code. Numeric identity fields cannot
inject text. RISC-V accepts only selected ISA names into a fixed bitmap, never
copies firmware strings into procfs, bounds numeric properties to 32/64-bit
cells, and uses the existing CPU map before indexing. Missing identity properties
remain unknown. Malformed firmware cannot create procfs lines or enable an
unrecognized extension. Firmware's hardware assertions remain trusted.

FP/SIMD claims require supported kernel context preservation; SVE, RISC-V V/Zve,
and LoongArch LSX/LASX are omitted. This reporting policy does not enable those
units, constrain userspace instruction execution, or change hwprobe.

`CpuInfo::hwcap()` converts the same usable set into the `AT_HWCAP` encoding for
one CPU, and `system_hwcap()` intersects the identified CPUs before exec
publishes the word. Both steps are conservative: a bit is announced only when
the kernel deems it usable and every online CPU advertises it, so user-selected
instruction paths stay valid after a migration. Only the aarch64 encoding is
produced; other architectures report zero, which is a claim of no capability
rather than a grant.

## x86_64 CPU capability reporting

CPUID is hardware/hypervisor input. `cpuinfo.rs` uses raw-cpuid's optional
getters to gate leaf queries and stores fixed-size identity data. The library
is no_std and introduces no allocation or new local unsafe boundary. CPUID
answers must remain stable during CPU-local bring-up; their correctness itself
remains trusted.

`cpuid_text` retains printable-ASCII validation, NUL termination and whitespace
normalization. The library's invalid UTF-8 vendor placeholder exceeds the
12-byte bounded destination and remains `unknown`; the brand placeholder is
explicitly rejected. Brand raw bytes are checked before accepting `as_str()`
because its trim would otherwise hide control characters at the ends. That
scan stops at the first NUL, including across leaf boundaries. A non-printable
byte before NUL produces `unknown`, so an injected newline cannot forge a flag
line. The vendor getter conservatively rejects invalid UTF-8 anywhere in its
12 bytes, including after NUL. A base/extended-family sum exceeding u8 is
rejected before calling the library's u8 addition; signature fields are then
unavailable rather than wrapping or panicking.

Per-CPU identity is published via `klazy::Once`, with no new unsafe code or
custom atomic publication protocol. Identification runs only on its owning CPU
before scheduling; readers never execute CPUID on behalf of a remote CPU and
never wait for an unstarted CPU. Missing snapshots confer no capabilities.

The capability API exposes only an explicit subset. It suppresses floating-point
and SIMD capabilities when their state is not managed. XSAVE-dependent flags
require the boot-selected state mask and their hardware dependencies. This
prevents procfs consumers from selecting AVX paths that cannot be preserved. It does not prevent
userspace from querying CPUID directly or executing unsupported instructions.
The snapshot is per CPU, not a system-wide intersection or scheduling guarantee.


## Overview

`kcpu` is a low-level crate in x-kernel that directly operates CPU privileged hardware, and it involves a large amount of
`unsafe` code: naked functions, inline assembly, raw pointer operations, and `transmute`. Incorrect
use or a broken invariant can lead to undefined behavior, memory corruption, or privilege leaks.

## Test Audit Scope

The audit scope covers the guest tests in `ut-tests/` and the host tests retained in `tests/host/`.
The guest files remain submodules of their original parent modules, and the original public, crate-private, or private visibility
is unchanged. The unsafe code, register accesses, per-CPU state restoration, and serial attributes in the tests all moved along with the original test bodies;
the production API is not widened and the production unsafe contracts are not changed. The two AArch64 host tests are still mounted by the
original `cfg(test)` and do not depend on guest registration.

## Trust Model

The LoongArch idle return fixup is performed only in the normal IRQ branch, when the saved PRMD.PPLV is kernel level,
and `karch` matches the address of the dedicated `idle` instruction exactly. The fixup does not accept a user address as a
jump target and does not change the saved IRQ mask. Its availability constraint is: after an interrupt is consumed to wake the CPU, it must
skip that wait instruction and return to the scheduler to re-check for work, rather than relying on the next periodic interrupt to break out.

```text
Kernel upper layers (scheduler/signals/syscalls/drivers)
   │
   │ safe API: init_trap(), UserContext::run(),
   │           unsafe API: TaskContext::switch_to(), user_copy()
   │
   v
┌──────────────────────────────────────────────────┐
│  kcpu                                            │
│                                                  │
│  ┌── unsafe boundary ──────────────────────────┐ │
│  │ asm entry (excp.S / copy_user.S)            │ │
│  │ naked function (context_switch, fpstate_*)  │ │
│  │ inline assembly (movgr2fr.d / movfr2gr.d)  │ │
│  │ Raw pointer operations (exception table, stack frame construction)  │ │
│  │ transmute (IDT fill, register array cast)   │ │
│  │ MSR/CSR writes (syscall config, page table switch)       │ │
│  └─────────────────────────────────────────────┘ │
└──────────────────────────────────────────────────┘
```

- **Callers of the safe API** trust `kcpu` to correctly maintain the register layout and the hardware operation invariants.
- **Assembly code** is regarded as an implementation detail within the trusted boundary, and its correctness is guaranteed by manual audit and architectural conventions.
- **Callers of `user_copy`** trust that the exception table mechanism can correctly intercept invalid user pointers.

## External Boundaries / Attack Surface

This module directly touches the following external boundaries:

- **User memory**: the `user_copy` function in `copy_user.S` reads and writes data at user-space addresses.
  The user may supply arbitrary addresses (including unmapped, read-only, or kernel addresses), and the exception table mechanism is used to recover safely.

- **MMIO / CSR / MSR registers**: the trap handler reads CR2 (x86_64),
  FAR_EL1 (aarch64), stval (riscv), and badv (loongarch64) to obtain the fault address.
  MSR writes configure the syscall entry (x86_64 LStar/Star/SFMask/Efer).

- **User-mode register state**: after `UserContext::run()` switches to user mode, the user can control
  the contents of all general-purpose registers. The trap handler must correctly handle arbitrary register values.
- **RISC-V kernel stack slot**: `ExceptionContext.kernel_sp` is outside `GeneralRegisters`, so signal
  frames neither expose it nor restore it from user-controlled register state.
- **riscv64 `gp` (percpu base)**: `gp` is a per-CPU invariant and is never restored from the per-task
  trapframe. During a trap, the task may migrate to another hart because the handler blocks; if `gp` were restored from
  a migratable trapframe, the percpu base of another hart would be loaded into this hart,
  making `current()`/`this_cpu_id()` point to the wrong CPU and task, which constitutes a cross-task pointer leak.
  Current implementation: an S-mode trap return does not touch `gp`; only a U-mode return restores the user
  `gp` from slot-3.

- **Interrupt/exception input**: hardware interrupts and exceptions trigger the trap entry. IRQ numbers come from hardware
  and are not fully trusted.

- **Instruction stream**: LoongArch64's `emulate_unaligned()` reads the instruction
  word at the fault address and decodes it. That instruction word comes from the user or kernel code segment.

## unsafe Code Inventory

### Current Task Pointer Publication

The storage and access of current are owned by the architecture module, and task liveness is guaranteed by the scheduler. The lower layer only passes an opaque
pointer; it does not dereference the task object and does not acquire or release references. The publisher must retain the new task and keep the old task alive
across the publication window and the period during which the old stack is in use; a borrow taken by an NMI must end before the interrupted flow resumes.

`TaskContext::switch_to` is an unsafe boundary: the caller guarantees that the next task pointer matches the target context,
that the context is held exclusively, and that IRQs and migration are excluded. The architecture layer publishes the
pointer after the hardware context is prepared and before the lower layer switches stacks, and it does not perform task destruction. Publishing current and switching the stack are not a single atomic action; an NMI may only rely on the
pointer pointing to a live task, and must not assume from it that all hardware contexts have been switched at the same time.

x86_64 uses aligned GS instructions and a compiler memory barrier, so CPU selection and the pointer access cannot be split by migration;
ARM64 reads SP_EL0 directly, and CPU selection and the read likewise cannot be split by migration. Boot clears this register
on both the EL1 and VHE paths. The EL0 exception entry restores current from this CPU's `ENTRY_TASK` before calling into Rust;
the hardware mask state at entry covers this handoff. On return to EL0, the last Rust call is completed first while current is still valid, and normal IRQs, pseudo-NMIs, and any enabled hardware NMIs are masked; only then is the
user SP written. The final `eret` restores the user interrupt state from SPSR. The user SP is not used as a task pointer.
RISC-V reads `tp` directly, and both the primary and secondary cores clear it at boot before entering Rust. The `tp` of user mode and guests
is used only in the assembly handoff window with IRQs disabled, and the host current is restored before returning to kernel Rust. The `tp` saved in the kernel
trapframe belongs to the task and can be restored even if the task migrates; `gp` belongs to the CPU and must not be
restored from a migratable kernel trapframe. This mechanism does not provide RISC-V NMI/RNMI entry support.
LoongArch reads `$tp` directly, and the primary and secondary cores clear it before the first Rust call. The user-mode handoff uses
a separate callee frame to save the kernel current and restores it while IRQs are still disabled, before returning to Rust;
the TLB refill assembly neither modifies `$tp` nor calls Rust. This mechanism does not add LoongArch NMI support.
The returned pointer does not carry any lasting CPU pinning or ownership. The interface must not be treated as a mechanism for acquiring a reference to a remote task.

### AArch64 Exception Return Window

An AArch64 hardware NMI may interrupt a normal IRQ handler, but must not interrupt the final ELR/SPSR restoration.
`HardwareNmiExitGuard` is declared first and dropped last; after the remaining exception cleanup is complete it sets ALLINT,
and keeps it set until `eret`; the saved SPSR is unchanged. This operation is gated by the current CPU's FEAT_NMI readiness,
and degraded operation does not access ALLINT. The return to user mode likewise masks hardware NMIs before replacing SP_EL0.

The boolean `in_exception_context()` query pins CPU-slot selection and load
with architecture IRQ save/restore. Permanent atomic storage alone does not
prevent a migrating caller from observing the previous CPU. This query does
not dereference the stored pointer; its guarantee does not extend to subsequent
caller operations or replace the trapframe snapshot's lifetime requirements.

### 1. `ExceptionContextGuard::new` / `Drop` (`active_exception_context.rs`)

```rust
ACTIVE_EXCEPTION_CONTEXT_PTR
    .current_ref_raw()
    .swap(ptr, Ordering::Relaxed)
```

**Invariant**: `tf` must be a valid `&ExceptionContext` reference; the lifetime of the guard
must cover the entire execution of the trap handler.

**Why it is safe**: the raw per-CPU slot address is recorded at installation; the per-CPU data area is valid for the entire kernel
lifetime, so this raw pointer can still be dereferenced at a late Drop. The `swap` is performed in the IRQ-disabled
context at trap entry. On Drop, CAS is used to restore the original slot and the current CPU slot after migration, avoiding
writing to or missing a CPU slot after the trap backend blocks or migrates. A CAS failure means the slot has been cleared by the scheduler or overwritten by a
new trap, and keeping the current value is the correct behavior. When the inner guard after migration is released, the current CPU slot
is restored to `self.prev` rather than 0; `self.prev` comes from the outer guard still alive in the same task, and must
remain visible until the outer guard itself is released.

### 2. `active_exception_context()` raw pointer conversion (`active_exception_context.rs:39-52`)

```rust
Some(unsafe { *(ptr as *const ExceptionContext) })
```

**Invariant**: `ptr` must point to a valid `ExceptionContext` instance.

**Why it is safe**: the pointer is installed only by `ExceptionContextGuard::new`, and the guard's `Drop`
attempts restoration on the original slot and the current CPU slot; before switching away from the installer task, the scheduler suspends and clears this
CPU's slot, and restores it to the current CPU after switching back. This API immediately copies the trapframe by value and returns a snapshot,
avoiding exposing to the caller a borrowed reference that could become invalid after the trap returns.

### 3. `prepare_initial_frame` (`x86_64/ctx.rs:367-379`)

```rust
let frame_ptr = unsafe { top_u64.sub(1).cast::<ContextSwitchFrame>().sub(1) };
unsafe { frame_ptr.write(ContextSwitchFrame { rip: entry as _, ..Default::default() }) };
```

**Invariant**: `kstack_top` points to a valid kernel stack top, and the stack has enough space to hold
`ContextSwitchFrame` (56 bytes).

**Why it is safe**: it is called by `TaskContext::init()`, and the caller guarantees that `kstack_top` is valid.
The x86_64 stack requires 16-byte alignment, and `sub(1)` guarantees alignment after `ret`.

### 4. `context_switch` naked function (`x86_64/ctx.rs:376-398`)

```rust
#[unsafe(naked)]
unsafe extern "C" fn context_switch(_current_stack: &mut u64, _next_stack: &u64) {
    naked_asm!("push rbp; push rbx; ... mov [rdi], rsp; mov rsp, [rsi]; ...")
}
```

**Invariant**: `rdi` points to the `rsp` field of the current task, and `rsi` points to the `rsp`
field of the next task. The callee-saved register layout is consistent with `ContextSwitchFrame`.

**Why it is safe**: interrupts are disabled before the call; the save/restore sequence is symmetric;
`ret` jumps to the entry function at `ContextSwitchFrame.rip`.

### 5. FP state save/restore (ctx.rs of each architecture)

- x86_64: standard `_xsave64` / `_xrstor64`; falls back to `_fxsave64` / `_fxrstor64` when XSAVE is absent.
- aarch64: `fpstate_save` / `fpstate_restore` naked functions.
- riscv: `save_fp_registers` / `restore_fp_registers` naked functions.
- loongarch64: `save_fp_registers` / `restore_fp_registers` naked functions.

**Invariant**: the size and alignment of the target memory region satisfy the architectural requirements.

**Why it is safe**: the layouts of `FpState`/`FpuState`/`FxStateBlock` are guaranteed by
`static_assertions` or `repr(C, align(...))` to match what the instructions expect.

On aarch64, the 32 128-bit vector registers occupy the first 512 bytes; `fpcr` and
`fpsr` are 32-bit fields at offsets 512 and 516. The naked assembly must access
both fields as 32-bit words and must not read or write the trailing alignment padding.

### 5a. x86_64 signal-frame FP/SIMD restore (`x86_64/xstate.rs`)

`rt_sigreturn` reads `ucontext` and the FP bytes with a checked copy at the syscall boundary.
It first validates the length (at most 836 bytes), both magics, the supported xfeatures, XSTATE_BV,
zero XCOMP_BV and the reserved fields, and then restores into the 64-byte-aligned kernel-owned `ExtendedState`.
No user pointer is ever passed directly to XRSTOR. A short frame, an unmapped frame, and an invalid header trigger process
SIGSEGV; RIP/RSP and the code segment are also validated before any register is changed, and RFLAGS permits only user-state bits.

MXCSR is still ANDed with the trusted boot-time mask under this project's existing compatibility policy (0xffbf is used
when the hardware returns 0), unlike Linux's policy of rejecting an invalid MXCSR. The BSP publishes the save format and mask;
each AP must verify compatibility and install the same XCR0 before it can be scheduled. New tasks, exec and signal handlers
use a deterministic initial state; signal capture uses fully initialized storage to avoid leaking unwritten extended regions.
The user can edit their own x87/SSE/YMM data but cannot enable AVX-512/AMX through the header.

### 6. `ExtendedState::default` (`x86_64/xstate.rs`)

`ExtendedState::default` is a safe `const fn` that builds the state from `FxStateBlock::initial()` and zeroed XSAVE header and YMM fields; it contains no `unsafe` operation. Every field is explicitly initialized, so no all-zero validity assumption is needed. The layout is guaranteed by `repr(C, align(64))` with compile-time size and offset assertions on `ExtendedState`, and by `repr(C, align(16))` with a 512-byte size assertion on `FxStateBlock`, matching what XSAVE/FXSAVE expect.

### 7. `switch_to` page table switch (`ctx.rs` of each architecture)

```rust
if next_ctx.cr3 != self.cr3 {
    karch::write_user_page_table(next_ctx.cr3);
}
```

**Invariant**: On architectures with a shared root, `cr3`/`satp` must point to a page table that is valid for the whole time the task runs;
on architectures with separate roots, the kernel page table is kept valid by the boot flow, and the `ttbr0_el1`/`pgdl` of a user task
must be backed by a corresponding page table held by the runtime. Kernel tasks keep the zero user root default and do not access the user address region.

**Why it is safe**: `TaskContext::new_kernel(kernel_root)` explicitly takes the permanent kernel root
and does not read the creator's current user root. x86_64/RISC-V save it as the task root; AArch64/LoongArch
keep the zero user root default. The constructor does not take ownership of the MM, so the caller must guarantee that the permanent kernel mapping
remains valid, and must install the user root it holds via `set_page_table_root()` before a user task is published.

### 8. `transmute` in IDT initialization (`x86_64/idt.rs:25-29`)

```rust
let entries = unsafe {
    core::mem::transmute::<&mut InterruptDescriptorTable, &mut [Entry<()>; 256]>(&mut table)
};
```

**Invariant**: The memory layout of `InterruptDescriptorTable` is identical to that of `[Entry<()>; 256]`.

**Why it is safe**: The `InterruptDescriptorTable` documentation declares it to be a
transparent wrapper around 256 `Entry` values. `const NUM_INT: usize = 256` guarantees that the sizes match.

### 9. `init_syscall` MSR writes (`x86_64/userspace.rs:169-191`)

```rust
LStar::write(...); Star::write(...); SFMask::write(...);
Efer::update(|efer| *efer |= EferFlags::SYSTEM_CALL_EXTENSIONS);
```

**Invariant**: The `syscall_entry` address must lie within the kernel code segment; the segment selectors must point to the correct
GDT descriptors.

**Why it is safe**: `syscall_entry` is defined in `excp.S` and linked into the kernel address space.
The segment selectors use compile-time constants from `gdt.rs`.

### 10. Exception table operations (`userspace_common.rs:52-90`)

```rust
let entries = unsafe {
    core::slice::from_raw_parts(start, len)
};
```

**Invariant**: `_ex_table_start` and `_ex_table_end` are defined by the linker script
and mark the start and end of the `__ex_table` section; the number of entries is non-negative.

**Why it is safe**: The symbols are generated by the linker and, after being sorted in `init_exception_table()`,
are only accessed read-only. The binary search in `fixup_exception` does not modify the entries.

### 11. LoongArch64 `emulate_unaligned` (`loongarch64/unaligned.rs:574-636`)

```rust
let badi = unsafe { core::ptr::read(self.era as *const u32) };
let regs = unsafe {
    core::slice::from_raw_parts_mut(
        core::ptr::from_mut(&mut self.regs).cast::<usize>(),
        32,
    )
};
```

**Invariants**:

- `self.era` must point to a readable memory address (the location of the faulting instruction).
- The memory layout of `GeneralRegisters` must be identical to that of `[usize; 32]`.
- `rd` (the destination register number) is in the range 0–31.
- The target address `badv` of the unaligned access can be safely accessed.

**Why it is safe**: It is called in the trap handler context, where `era` points into the code segment containing
the faulting instruction. `GeneralRegisters` is a `repr(C)` struct whose 32 `usize` fields are laid out contiguously.
The register number is extracted from the low 5 bits of the instruction word, so it is inherently in the range 0–31.

### 12. LoongArch64 FP register reads and writes (`loongarch64/unaligned.rs:67-533`)

```rust
unsafe { asm!("movgr2fr.d $f0, {val}", val = in(reg) val) }
unsafe { asm!("movfr2gr.d {val}, $f0", val = out(reg) value) }
```

**Invariant**: Each assembly instruction operates on a fixed FP register number, in one-to-one correspondence with the match arms of `write_fpr`/`read_fpr`.

**Why it is safe**: In the LoongArch64 ISA, `movgr2fr.d` / `movfr2gr.d` are data-move instructions between general-purpose registers
and FP registers and involve no memory access. Each of the 64 functions hard-codes
its own register number from $f0 to $f31.

### 13. `NmiExceptionGuard::drop` (`aarch64/excp.rs`)

```rust
unsafe { core::arch::asm!("msr daifset, #2", options(nomem, nostack)) };
```

**Invariant**: The guard performs restoration only when `pmr_active` is set (the entry PMR has been saved and the mask enabled);
`msr daifset` masks only IRQ, and what is written back afterwards is the `ICC_PMR_EL1` saved by the same CPU at entry.

**Why it is safe**: The execution context is exception entry/exit (this CPU, IRQs disabled); the PMR value belongs to the current
CPU; `eret` then restores the pre-interrupt DAIF state from SPSR.

## Memory Safety Invariants

The following invariants must hold at all times:

1. **Stack frame layout consistency**: The register layout saved by the assembly entry must exactly match the `repr(C)` layout of the Rust-side
   `ExceptionContext`. The `trapframe_size` compile-time
   constant is injected into the assembly to ensure the sizes agree.

2. **Page table validity**: The permanent kernel root passed to `new_kernel(kernel_root)` must remain valid.
   Architectures with a shared root save it directly; kernel tasks on architectures with separate roots keep the zero user root and do not access the user address region.
   A user task must install the valid user root held by the runtime before it is published. Before writing to the hardware, `switch_to` does not
   verify the page table or hold an MM reference; the caller guarantees that the lifetime and the use of the root are consistent.

3. **Exception table is sorted**: `fixup_exception` uses binary search and requires that the exception table
   has been sorted by `init_exception_table()` before the call.

4. **Guard lifetime and migration**: `ExceptionContextGuard` must cover the entire execution of the trap handler.
   The guard records the per-CPU slot at install time; before the scheduler switches away from the installer task, it must suspend and
   clear the current CPU slot, and restore it on the current CPU after switching back. Drop must handle both the original slot and the current CPU slot after migration,
   to avoid leaving a stale active-exception marker after the trap handler blocks/migrates,
   or losing the exception context marker after execution resumes. The `prev` of a nested guard points to the `ExceptionContext` of the outer live guard;
   as long as the outer guard has not been released, this pointer must be treated as a marker that the current task is still in
   the outer exception context.

5. **Interrupts disabled**: The core operations of `context_switch` and `UserContext::run` must be performed with
   interrupts disabled. Disabling interrupts protects per-CPU state and the atomicity of register save/restore.

6. **Naked function calling convention**: The argument passing of all naked functions must be consistent with the register
   usage in the assembly (for example, `rdi`/`rsi` on x86_64 for `context_switch`).

## Thread Safety

| Type | `Send` condition | `Sync` condition |
| ---- | ----------- | ----------- |
| `ExceptionContext` | A `Copy` type, automatically `Send` | Automatically `Sync` |
| `UserContext` | `Copy` / all fields are `Send` | Automatically `Sync` |
| `TaskContext` | All fields are `Send` | All fields are `Sync`, but it should not be shared across CPUs |
| `ExceptionContextGuard` | Should not be transferred across threads (bound to the install-time slot) | Must not be shared |

- `TaskContext::switch_to` is an unsafe method; the caller guarantees exclusive access to the running and target contexts,
  and rules out ordinary IRQs, migration, and concurrent execution on other CPUs. `&mut self` alone cannot prove these conditions.
- `ACTIVE_EXCEPTION_CONTEXT_PTR` is a per-CPU variable. The install path writes the current CPU's copy;
  a late Drop may access the original CPU's slot after migration, and may also need to clear the restore
  marker of the post-migration current CPU, so the restore path must use CAS and must not unconditionally overwrite either CPU's slot.
- The `IRQ` and `PAGE_FAULT` distributed slices are compile-time static data and read-only at runtime.

## Threat Analysis

| ID | Threat Description | Impact Level | Trigger Condition | Mitigation |
| ---- | -------- | -------- | -------- | -------- |
| T-01 | User passes a malicious address through `user_copy`, causing a kernel panic | High | `user_copy` accesses an invalid user pointer and the exception-table entry is missing | The exception-table mechanism covers all memory-access instructions in `copy_user.S`; `fixup_exception` intercepts kernel-mode Page Faults and resumes on a safe path |
| T-02 | trap frame layout inconsistent with assembly causes register misalignment | High | `ExceptionContext` field order changed without updating the assembly | The `trapframe_size` compile-time constant is injected into the assembly; `repr(C)` keeps the memory layout predictable |
| T-03 | LoongArch64 `emulate_unaligned` decodes an unknown instruction and corrupts registers | High | The faulting instruction has an unexpected opcode | An unmatched opcode returns `Err(UnalignedError)` without modifying registers; the trap handler panics instead of continuing |
| T-04 | `active_exception_context()` returns an expired or stale trapframe | High | The original CPU slot is not cleaned after the trap handler blocks/migrates, or the caller persists the snapshot source | The API returns only a by-value snapshot; context switch suspends and clears the current CPU slot and restores it after switching back; the guard's Drop uses CAS to restore the original/current slot |
| T-05 | `switch_to` uses an invalid page-table root, causing TLB fills of wrong addresses | High | `TaskContext`'s page-table root is set incorrectly | `new_kernel` explicitly takes a permanent root; user roots are installed by `set_page_table_root`, and the caller guarantees page-table validity |
| T-06 | IDT/vector table entry points to a wrong address | High | Misspelled assembly symbol name or a linker-script problem | `trap_handler_table` is a fixed-size array defined by assembly; IDT initialization fills it via `set_handler_addr` |
| T-07 | `switch_to` or `run` called with interrupts not disabled | Medium | Caller violates the calling constraints | `run()` calls `disable_local_irq()` internally; `switch_to` requires the caller to guarantee interrupts are disabled |
| T-08 | `ExceptionContextGuard` nesting overwrites the per-CPU pointer | Low | Another exception is triggered while a trap handler is running | `swap` semantics save the previous value and the guard restores it with CAS on drop; deep nesting is restored correctly through the guard chain and does not overwrite a later new trap |
| T-09 | LoongArch64 user mode crafts a malicious unaligned instruction word | High | A user program performs an unaligned access and the faulting instruction word is decoded by `emulate_unaligned` into an unexpected operation | `emulate_unaligned` modifies only the destination register specified in the instruction encoding; an unmatched opcode returns an error without modifying any state; the opcode masks match only known instruction types |
| T-10 | `init_trap()` called repeatedly leaves hardware state inconsistent | Medium | A second call overwrites already-active hardware structures such as GDT/IDT/vector tables | The caller guarantees a single call (single-threaded execution in the boot phase); no runtime protection |
| T-11 | x86_64 `orig_rax` maliciously tampered with, causing abnormal syscall restart | Medium | User mode modifies `orig_rax` in the trap frame through a signal handler | `orig_rax` is written only in the `syscall_entry` assembly; the x86_64 signal frame exports only Linux ABI state and resets `orig_rax` on restore; x86_64 `UserContext::kernel_rsp` also does not enter the user signal frame; signal delivery code decides the restart condition via `syscall_restart_error()` |
| T-12 | `rollback_syscall` called in a non-syscall context | Low | Signal handling code misjudges the context type | `is_from_syscall()` checks `orig_rax != u64::MAX`; in a non-syscall context `rollback_syscall()` and `restart_with_syscall()` are no-ops |
| T-13 | IRQ misclassified as NMI (or vice versa) | High | `use_nmi_path` misjudges SPSR.I / entry PMR, or the readiness flags (`pmr::is_ready` / `allint_active`) are wrong; impact: an NMI handler runs under wrong context constraints (e.g. a normal IRQ treated under NMI rules), or a Superpriority NMI is treated as a normal IRQ and loses its priority | Classification is double-gated by a hardware-state snapshot plus readiness; degraded builds never touch NMI registers; the NMI path keeps ALLINT/PMR nesting protection |
| T-14 | Malicious x86_64 FP signal frame makes XRSTOR raise a kernel exception | High | The user tampers with `MXCSR`, the XSAVE header, the frame length or pointers; impact: a kernel exception or a data leak | checked copy, maximum-length limit, feature/header/magic validation, trusted MXCSR mask, 64-byte-aligned private storage; an invalid frame delivers SIGSEGV |

## Failure Modes and Effects Analysis (FMEA)

| ID | Failure Mode | Failure Cause | Local Effect | System Effect | Severity | Mitigation |
| ---- | -------- | -------- | -------- | -------- | ------ | -------- |
| F-01 | A Page Fault cannot be handled, causing a panic | The page is unmapped + no matching exception table entry + no registered handler | The current CPU halts | System crash | 1 | Three-level handling: registered handler → exception table fixup → panic with diagnostic information |
| F-02 | `user_copy` returns a non-zero value (partial copy) | Part of the user buffer is invalid | The system call returns -EFAULT | The user process receives an error and may retry | 3 | The caller checks the return value; the exception table mechanism guarantees no panic |
| F-03 | `emulate_unaligned` returns an error | The faulting instruction is an unsupported FP or vector operation | The unaligned access fails | The current process may receive SIGBUS | 3 | The trap handler panics (kernel mode) / returns an error (user mode) |
| F-04 | An interrupt during a context switch corrupts registers | The caller did not disable interrupts | The current task state is inconsistent | Scheduler data is corrupted | 1 | The API contract requires calling with interrupts disabled; `run()` manages the interrupt state itself |
| F-05 | Wrong GDT/IDT initialization order | per-CPU is uninitialized before `init_trap()` is called | Segment register loading fails | CPU exception | 1 | The documentation states the precondition; the boot code calls in a fixed order |
| F-06 | FP state leaks across tasks | `fp-simd` is not enabled but a task uses FP instructions | FP registers contain the previous task's data | Information leak | 2 | Without `fp-simd`, nothing is saved/restored; relies on the kernel configuration to disable user-mode FP |
| F-07 | `ExceptionContextGuard` is dropped early | The guard is shadowed or manually dropped in the trap handler | `active_exception_context()` returns a wrong pointer | The NMI watchdog obtains an expired snapshot | 2 | The guard is declared as the first local variable of the handler, and its lifetime covers the entire function |
| F-13 | A late drop of `ExceptionContextGuard` writes the wrong CPU slot | The trap handler blocks and migrates, and Drop recomputes the current CPU | The original CPU keeps a stale active-exception marker for a long time, or the current CPU marker is not restored after migration | `check_preempt_pending()` skips preemption indefinitely, or the second half of the trap handler wrongly allows preemption | 1 | The guard saves the original slot and restores it with CAS; a context switch suspends/restores the current CPU slot |
| F-08 | `emulate_unaligned` access failure | The unaligned instruction decodes successfully but the actual access faults (page not present) | `unaligned_read`/`unaligned_write` see -1 from the assembly helper and `emulate_unaligned` returns `Err(UnalignedError)` via `?` | The caller (e.g. the trap handler) decides how to handle the error; in kernel mode it may panic | 1 | The `_unaligned_read`/`_unaligned_write` assembly helpers have exception table entries and return an error on failure instead of panicking inside the function |
| F-09 | A panic inside the trap handler triggers a double fault | Another exception is triggered during the panic | The panic unwind path accesses invalid memory | The system is unrecoverable | 1 | The panic handler should minimize its operations; x86_64 #DF has a separate stack (TSS IST) |
| F-10 | Repeated calls to `init_trap` reset the IDT/vector table | Boot code logic error | Interrupt configuration is overwritten | Subsequent interrupt handling is abnormal | 1 | Precondition constraint (call only once); no runtime protection |
| F-11 | `syscall_restart_error` misjudges, causing an unnecessary syscall restart | The return value happens to equal an ERESTART constant but the context is not a syscall | The user-mode syscall is restarted unexpectedly | The user process behaves abnormally | 3 | The `is_from_syscall()` precheck filters out non-syscall contexts; the restart constants are negative values, which normal return values do not match |
| F-12 | `rollback_syscall` rolls rip back to an illegal address | The `rip` value is less than 2 (the length of the syscall instruction) | `saturating_sub` guarantees no overflow, but after the rollback rip may point to an illegal instruction | The user process receives SIGILL | 3 | `saturating_sub` prevents underflow; the user-mode rip is set by the kernel and is normally never less than 2 |

## Fault Management

`kcpu` handles faults through the following mechanisms:

- **Exception table fixup**: Page Faults in `user_copy` are safely recovered through the exception table, returning
  the number of uncopied bytes (0 means success).
- **Distributed handler chain**: Page Fault and IRQ handlers are registered through `linkme` distributed
  slices and are tried in registration order.
- **panic**: Unrecoverable exceptions (unknown vectors, unhandled Page Faults, #GP, etc.)
  panic directly, with the complete trap frame and backtrace information.
- **`Result` propagation**: The LoongArch64 `emulate_unaligned` returns
  `Result<(), UnalignedError>`, allowing the caller to decide how to handle it.

## Privacy Analysis

This module does not process the content of user data. `user_copy` moves data between the kernel and user space,
but does not parse or record the data content. FP registers may contain sensitive user-mode data;
once the `fp-simd` feature is enabled, save/restore prevents leakage across tasks.

## Known Limitations

1. **The trap handler does not support nesting**: `ExceptionContextGuard` uses a single per-CPU
   pointer; deeply nested exceptions only restore the previous value through the guard chain and do not maintain a complete stack.

2. **At most one IRQ handler**: The `handle_trap!` macro calls only the first registered
   handler; when there are multiple handlers it prints a warning and ignores the rest.

3. **LoongArch64 unaligned emulation has limited coverage**: It covers only basic load/store and scalar floating-point instructions,
   and does not cover unaligned accesses by SIMD (LSX/LASX) instructions.

4. **`context_switch` has no stack overflow detection**: The kernel stack of a new task must have enough space to hold
   `ContextSwitchFrame`, otherwise `prepare_initial_frame` writes out of bounds.

5. **x86_64 extended-state scope**: only x87/SSE/YMM are supported; AVX-512, AMX and supervisor
   state are neither enabled nor reported. APs must be compatible with the BSP's save format; no heterogeneous-CPU degradation or hotplug negotiation is provided.

6. **The riscv64 hwprobe snapshot depends on firmware descriptions**: The RISC-V capability snapshot
   reports only information that can be determined from the device tree or the kernel address layout. Undescribed vendor ids
   and extensions are returned as `0` / unknown according to the Linux hwprobe ABI, to avoid reading M-mode CSRs that may fault
   on the S-mode syscall path.

## Audit Checklist

When modifying `kcpu`, verify the following:

- [ ] After modifying `ExceptionContext` fields, update `trapframe_size` and the
      register save/restore sequences in the assembly accordingly.
- [ ] Every newly added `unsafe` block has a `SAFETY:` comment stating its invariants.
- [ ] After modifying the `ContextSwitchFrame` layout, update the `push`/`pop` sequences
      in the `context_switch` naked function accordingly.
- [ ] After modifying the GDT segment selector constants, verify the `cs`/`ss` values in `UserContext::new`.
- [ ] When adding handling for a new exception vector, check whether the exception table needs updating or a new handler needs to be registered.
- [ ] When adding new instruction emulation on LoongArch64, verify the opcode masks and the register number ranges.
- [ ] After modifying `copy_user.S` / `atomic_user.S`, confirm that every memory-access instruction has a corresponding `_asm_extable` entry.
- [ ] When adding a new per-CPU variable, verify the initialization order (percpu init → `init_trap`).
- [ ] When adding a new RISC-V hwprobe key, confirm whether its data comes from the boot-time snapshot,
      and update the `ksyscall` aggregation/filtering semantics accordingly.
- [ ] After modifying the page table switching logic, verify that the TLB flush semantics are correct.
- [ ] After modifying code gated by the `fp-simd` feature, verify consistency across all architectures.
- [ ] After modifying x86_64 `orig_rax` or the syscall restart logic, verify that the push/pop sequences in `excp.S` are consistent with the `ExceptionContext` layout.
- [ ] When adding a new restartable syscall error code, update the constant list in `syscall_restart_error()` accordingly.

## Interrupt Callback Safety

`src/excp.rs` exposes unsafe IRQ/NMI callback pointers. Invocation sites in each
architecture's `excp.rs` and `userspace.rs` rely on hardware trap masking and
saved CPU state; user-return callbacks must retain the existing IRQ-off contract.
The captured stack remains valid across the ordinary HAL scheduling checkpoint.
NMI entry retains ordinary IRQ/migration exclusion until return and does not
authorize scheduling. Page-fault callbacks do not inherit this unsafe signature.

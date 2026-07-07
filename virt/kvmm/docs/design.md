# kvmm — Kernel Virtual Machine Monitor

## Overview

`kvmm` is x-kernel's lightweight type-1 hypervisor module, providing hardware-assisted
virtualization across three architectures:

| Architecture | Backend | Privilege Model |
|---|---|---|
| AArch64 | VHE (EL2 host) | Guest at EL1, host at EL2 |
| RISC-V | H-extension | Guest in VS-mode, host in HS-mode |
| x86_64 | VMX (Intel VT-x) | Guest in VMX non-root, host in root mode |

All three backends share a common `VmmArch` trait interface and a unified vCPU run-loop.

---

## Architecture

```
virt/kvmm/src/
├── lib.rs            # crate root, kvmm_init() entry point
├── vcpu.rs           # Vcpu<A> generic struct + vmm_run_vcpu loop
├── vm.rs             # Vm<A> container (VmConfig, vcpu array)
├── selftest.rs       # Generic self-test (100-iteration guest)
└── arch/
    ├── mod.rs        # VmmArch trait definition
    ├── aarch64/
    │   ├── mod.rs        # Aarch64Vhe impl, exit handler
    │   ├── el2_vmcs.S    # EL2 context switch
    │   ├── vcpu_ctx.S    # sysreg save/restore
    │   ├── guest_vec.S   # EL2 exception vectors for guest trap
    │   └── guest_test.S  # Selftest guest (WFI + HVC loop)
    ├── riscv64/
    │   ├── mod.rs        # RiscvHext impl, exit handler
    │   ├── hext_vcpu.S   # HS↔VS context switch
    │   └── guest_test.S  # Selftest guest (WFI + ecall loop)
    └── x86_64/
        ├── mod.rs        # X86Vmx impl, exit handler
        ├── vmx.rs        # VMCS fields, VMXON/VMCS init, helpers
        ├── vmx_run.S     # vmlaunch/vmresume entry/exit
        └── guest_test.S  # Selftest guest (HLT + VMCALL loop)
```

### VmmArch Trait

```rust
pub trait VmmArch {
    type ArchVcpu: Default + Send;

    fn init_vcpu(vcpu: &mut Vcpu<Self>, entry: u64, sp: u64) -> bool;
    fn restore_guest_ctx(vcpu: &mut Vcpu<Self>);
    fn enter_guest(vcpu: &mut Vcpu<Self>) -> bool;
    fn exit_handler(vcpu: &mut Vcpu<Self>) -> ExitAction;
    fn save_guest_ctx(vcpu: &mut Vcpu<Self>);
    fn guest_test_entry() -> u64;
    fn percpu_hw_init() -> bool { true }
}
```

`init_vcpu` provides a unified interface to set up guest entry point and stack pointer,
translating them into arch-specific registers (ELR/SP_EL1, vsepc/sp, VMCS RIP/RSP).

`guest_test_entry` returns the architecture's selftest guest entry point address.
`percpu_hw_init` performs per-CPU hardware initialization (idempotent) — defaults to
a no-op for architectures that need none (e.g. AArch64 VHE).

### vCPU Run Loop (`vmm_run_vcpu`)

```
restore_guest_ctx()
loop {
    enter_guest()       → vmlaunch / sret / eret
    save_guest_ctx()
    exit_handler()      → Resume / VmExit / Exit
}
```

### Selftest Flow

The selftest is architecture-generic — `selftest_impl<A: VmmArch>()` and
`selftest_smp_impl<A: VmmArch>()` are parameterized by the `VmmArch` trait,
using `A::guest_test_entry()` and `A::percpu_hw_init()` to dispatch per-arch
setup. A `CurrentArch` type alias (cfg-selected) maps to the concrete backend.

The selftest has two modes:

**Single-vCPU** (`vmm_selftest`): Creates one VM with one vCPU on the boot CPU.
Uses a static 16 KB guest stack.

**SMP multi-vCPU** (`vmm_selftest_smp`): Spawns `VCPUS_PER_CPU` (2) vCPUs per
physical CPU. Each vCPU thread:
1. Pins itself to a target CPU via `ktask::set_current_affinity`
2. Performs per-CPU hardware init (idempotent — safe when multiple vCPUs share a CPU)
3. Allocates a private 4 KB stack page via `kalloc::GlobalPage::alloc_zero()`
4. Initializes vCPU state with `init_vcpu(entry, sp)`
5. Runs the guest via `vmm_run_vcpu`

Both modes use the same guest test program: 100 iterations of yield + print hypercall
every 20 iterations + done hypercall at iteration 100.

### Per-CPU and Per-vCPU State

Multi-CPU support requires careful separation of per-CPU and per-vCPU state:

| Layer | AArch64 | RISC-V | x86_64 |
|-------|---------|--------|--------|
| Per-CPU hw init | none (VHE transparent) | `hext_init()` — set `hstatus.VTW` | `vmx_percpu_init()` — CR fixup + VMXON |
| Per-CPU state | — | hstatus CSR | VMXON region (`#[percpu::def_percpu]`) |
| Per-vCPU state | `Aarch64Vcpu` (GPRs, sysregs, host_vbar, host_tpidr) | `RiscvVcpu` (GPRs, CSRs) | `X86Vcpu` (GPRs, VMCS page) |

Key design decisions:
- **AArch64**: `host_vbar` and `host_tpidr` moved from global BSS into the per-vCPU
  struct, eliminating cross-CPU data races.
- **x86_64**: VMXON page allocated dynamically per CPU (not static BSS). VMCS page
  allocated per vCPU inside `init_vcpu`. Idempotency via `VMXON_DONE` percpu flag.
- **RISC-V**: All state is CSR-based and naturally per-hart. `hext_init()` is idempotent.

---

## Implementation History

### Phase 1: AArch64 VHE

Ported EL2 context-switch and trap vector from avatar-next. The host runs at EL2 with
`HCR_EL2.E2H=1` (VHE). Guest enters EL1 via `eret` and traps back on
WFI (`EC=0x01`) or HVC (`EC=0x16`).

### Phase 2: RISC-V H-extension

Ported HS/VS-mode switch from avatar-next. Guest runs in VS-mode with `vsatp=0`
(no stage-2 translation). WFI traps via `hstatus.VTW=1`.

### Phase 3: x86_64 VMX

Implemented VMCS initialization from scratch (referencing avatar-next for structure):
- `vmx_check_support()` — CPUID.1.ECX[5] + IA32_FEATURE_CONTROL check
- `vmx_percpu_init()` — Per-CPU CR4.VMXE, CR0/CR4 fixup, dynamic VMXON region allocation
- `vmcs_init_vcpu()` — Per-vCPU VMCS allocation and full field programming:
  - VM-execution controls negotiated via capability MSRs
  - Host state: CR0/CR3/CR4, segment selectors, GDT/IDT/TSS bases, FS/GS bases
  - Guest state: shares host CR3 (identity-mapped), RIP/RSP from selftest

### Phase 4: Multi-CPU Support

Refactored all three backends to eliminate global mutable state:

- **AArch64**: Moved `host_vbar` / `host_tpidr` from `.bss` globals into per-vCPU
  `Aarch64Vcpu` struct. Assembly offsets updated accordingly.
- **x86_64**: Replaced static VMXON BSS with `#[percpu::def_percpu]` dynamic allocation.
  Moved VMCS from shared state into per-vCPU `X86Vcpu`. Added `VMXON_DONE` percpu flag
  for idempotent init.
- **RISC-V**: Already per-hart (CSR-based), no changes needed.
- **VmmArch trait**: Added `init_vcpu(vcpu, entry, sp) -> bool` unified interface.
- **Selftest**: Added `vmm_selftest_smp()` — 2 vCPUs × N CPUs with CPU pinning.

---

## Bugs Encountered and Fixes

### 1. RISC-V: `R_RISCV_PCREL_HI20 out of range` for `__global_pointer$`

**Symptom**: Linker error when building hext_vcpu.S.

**Root cause**: The assembly referenced `__global_pointer$` to restore the `gp` register
on return from VS-mode. x-kernel's linker script does not define this symbol (no GP
relaxation), causing an out-of-range relocation.

**Fix**: Save host `gp` into `host_ctx[15]` on guest entry and restore from there on exit,
instead of recomputing from the undefined linker symbol. Added `HCTX_GP` offset at 448.

### 2. RISC-V: Unhandled `scause=22` (virtual instruction)

**Symptom**: Guest traps with cause 22, exit handler falls into the unhandled case.

**Root cause**: With `hstatus.VTW=1`, WFI in VS-mode generates `scause=22`
(virtual instruction exception), NOT `scause=2` (illegal instruction). The code originally
matched only cause 2 based on a misreading of the spec.

**Fix**: Added `CAUSE_VIRTUAL_INSTRUCTION = 22` constant and matched it in the exit handler.

### 3. RISC-V: Guest hangs after iter=20

**Symptom**: Selftest prints `iter=20` then stops responding.

**Root cause**: A timer interrupt becomes pending while in VS-mode. On trap to HS-mode,
`sstatus.SIE` is automatically cleared. The interrupt stays pending forever because the
kernel timer handler never gets invoked. On guest re-entry, the pending interrupt
immediately traps again — creating an infinite loop.

**Fix**: After detecting an interrupt exit, briefly enable `sstatus.SIE` so the kernel's
interrupt handler can service the pending interrupt:
```rust
core::arch::asm!(
    "csrsi sstatus, 0x2",  // SIE=1
    "csrci sstatus, 0x2",  // SIE=0
);
```

### 4. AArch64: LOG_LEVEL_INFO not applied

**Symptom**: `make run` shows WARN-level output despite defconfig requesting INFO.

**Root cause**: kconfig comment format was wrong. `# LOG_LEVEL_WARN=y` is not a valid
"unset" marker — it gets parsed as a comment and the option defaults to true. The correct
format is `# LOG_LEVEL_WARN is not set`.

**Fix**: Fixed all platform defconfigs to use correct kconfig unset syntax.

### 5. x86_64: `cannot use register bx: rbx is used internally by LLVM`

**Symptom**: Compile error in `cpuid()` helper.

**Root cause**: LLVM reserves `rbx` as a frame pointer on some configurations. Inline asm
cannot use `ebx`/`rbx` as an operand.

**Fix**: Manually push/pop `rbx` around the CPUID instruction and copy the result to a
generic register:
```rust
asm!(
    "push rbx",
    "cpuid",
    "mov {ebx_out:e}, ebx",
    "pop rbx",
    ...
);
```

### 6. x86_64: `unknown token in expression push %rbp`

**Symptom**: Assembly error in vmx_run.S when included via `global_asm!`.

**Root cause**: `global_asm!` on x86_64 defaults to Intel syntax, but the .S files use
AT&T syntax.

**Fix**: Added `options(att_syntax)` to all `global_asm!(include_str!(...))` calls.

### 7. x86_64: Triple fault on `vmlaunch` (instant QEMU reboot)

**Symptom**: QEMU reboots immediately after vmlaunch — the VM-exit handler crashes.

**Root causes** (two bugs):

1. **HOST_BASE_GS = 0**: The VMCS had `HOST_BASE_GS_ADDR = 0`, but x-kernel uses GS base
   for per-CPU data. After VM-exit restores GS base to 0, any per-CPU access triples.
   **Fix**: `vmcs_write(HostBaseGs, rdmsr(MSR_GS_BASE))`.

2. **`launched` flag never set**: After the first successful `vmlaunch`, the flag stayed
   false, causing every subsequent entry to use `vmlaunch` instead of `vmresume` on an
   already-launched VMCS — which fails.
   **Fix**: Added `movb $1, 0x84(%r15)` in `kvmm_vmx_return` to set the launched flag
   after each successful VM-exit.

---

## Current State

All three architecture selftests pass in both single-vCPU and SMP modes:
- **AArch64**: 100 iterations of WFI + HVC, verified on QEMU `virt` with EL2
- **RISC-V**: 100 iterations of WFI + ecall, verified on QEMU `virt` with H-extension (4 CPUs × 2 vCPUs)
- **x86_64**: 100 iterations of HLT + VMCALL, verified on QEMU with `-cpu max,+vmx` (4 CPUs × 2 vCPUs)

### Multi-CPU Support (Completed)

Each vCPU runs as a `ktask` kernel thread. Multiple vCPUs can share a physical CPU,
and vCPUs can be distributed across all available CPUs via affinity pinning.

### ktask Integration (Completed)

Each vCPU runs as a `ktask` kernel thread via `spawn_vcpu_thread()`. From the scheduler's
perspective, a vCPU thread is identical to any other kernel thread:

- **Preemption**: Host timer interrupts force VM-exits; the scheduler can then preempt the
  vCPU thread at any timer tick, regardless of guest behavior.
- **Cooperative yield**: WFI/HLT exits call `ktask::yield_now()` so idle guests release
  their time slice early (optimization, not required for correctness).
- **API**: `spawn_vcpu_thread<A>(vcpu) -> KtaskRef` — moves the Vcpu into the thread,
  caller uses `.join()` to wait (returns 0 on success, 1 on error).

Current limitations:
- Guest shares host CR3/page tables (no stage-2 / EPT isolation)
- No device emulation or interrupt injection
- Selftest-only (no user-facing VM creation API)

---

## Future Work

### 1. Stage-2 / EPT Memory Mapping

Implement guest-physical-to-host-physical translation:
- **AArch64**: Stage-2 page tables (VTTBR_EL2)
- **RISC-V**: hgatp (hypervisor guest address translation)
- **x86_64**: Extended Page Tables (EPT)

This isolates guest memory from the host and other guests. Required for running
untrusted guest code.

### 2. `/dev/kvm`-style Interface

Expose VMM functionality to userspace via a device node or syscall interface:
- `VM_CREATE` — allocate a VM with memory region descriptors
- `VCPU_CREATE` — add vCPUs to a VM
- `VCPU_RUN` — enter guest execution, return on exit
- `VCPU_SET_REGS` / `VCPU_GET_REGS` — read/write vCPU state

### 3. Interrupt Injection

Support injecting virtual interrupts into the guest:
- **AArch64**: Write to ICH_LR_EL2 (vGIC list registers)
- **RISC-V**: Set hvip bits for VS-level interrupts
- **x86_64**: VM-entry interrupt injection via VMCS interrupt-info field

### 5. Device Emulation

Implement MMIO/PIO trap-and-emulate for basic devices:
- Virtual UART (console I/O)
- Virtual timer
- Virtio transport (for block/net devices)

### 6. Multi-vCPU VM and IPI

Support VMs with multiple vCPUs belonging to the same VM:
- Inter-vCPU interrupt delivery (virtual IPI)
- vCPU migration between physical CPUs

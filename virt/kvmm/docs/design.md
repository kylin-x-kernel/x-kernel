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

### vCPU Run-State Machine (`VcpuRunState`)

`VmShared` publishes a coarse per-vCPU execution state, maintained by the run
loop (`vmm_run_vcpu`) and the WFI handler:

| State | Meaning |
|-------|---------|
| `Offline` | not started, or exited |
| `RunningGuest` | executing guest code at EL1 |
| `HostHandlingExit` | trapped out; host is handling the exit |
| `WfiSleeping` | parked in the VMM WFI path in an *interruptible* sleep |

The WFI path (`handle_wfi`) now parks the vCPU with
`ktask::interruptible_sleep_until` instead of a plain sleep, so an injected
virtual IRQ can wake it early. `VmShared::set_vcpu_task` records the owning
`KtaskRef` so `inject_irq` can reach the thread.

### Preemption-safe world switch

The per-vCPU state that lives in *per-physical-CPU* hardware — the Stage-2/EPT
root (VTTBR/EPTP/hgatp), the guest EL1 register bank, and (once added) the GICH
list registers — is loaded, entered, and read back inside a single IRQ-masked
window in `vmm_run_vcpu` (`ksync::spin::IrqSave`). Without it the scheduler
could preempt the vCPU thread between the Rust-side load and the `eret` and
migrate it to another pCPU, so the guest would resume on a CPU holding another
vCPU's hardware state (corrupt Stage-2, lost registers, or a dropped
just-injected IRQ). The guard is released before `exit_handler`, which may
block or yield. This extends the narrow `tpidr_el2` masking already done inside
`el2_enter_guest` to cover the whole setup/read-back.

**`inject_irq` is a skeleton in this stage.** It only wakes a vCPU that is
`WfiSleeping`. The actual injection substrate (recording a pending bit and
programming it into a list register) and the cross-pCPU IPI kick for a vCPU
running guest code on another physical CPU are the subject of the next stage
(see *Future Work → Interrupt Injection*). Until then, guests receive no
virtual interrupts.

### AArch64 world-switch / thread fixes

The EL2 exception registers are not vCPU-private and can be clobbered by a
host exception once the exit handler unmasks IRQs. They are therefore captured
in assembly *before* returning to Rust:

- **ESR_EL2** → `Aarch64Vcpu.esr` (offset 528), captured in the guest vector
  (`guest_vec.S`).
- **FAR_EL2 / HPFAR_EL2** → `Aarch64Vcpu.far` / `.hpfar` (offsets 536 / 544),
  captured in `el2_trap_exit_common` (`el2_vmcs.S`).

`el2_trap_exit_common` also:

- restores host `VBAR_EL2` *before* the `HCR_EL2` switch back to host mode,
  closing the window where a host EL2 exception would reuse the guest exit path
  and stale `tpidr_el2` (visible when several vCPUs share one physical CPU); and
- clears `PSTATE.PAN` (`msr pan, #0`) before returning, since a guest exit may
  leave PAN set while the host `copy_user` path expects it clear.

### Introspection (`/proc/kvmm`) and guest loader

Every `Vm` registers a `Weak<dyn VmInfo>` in a global registry on creation and
clears its `active` flag via `shutdown()` when torn down. `kvmm::dump_vm_info()`
walks the registry and formats a live snapshot — per-vCPU pCPU/run-state, cycle
accounting (guest vs host time, utilisation), an exit-reason breakdown
(halt/hcall/mmio/irq/other), and emulated MMIO devices. It is surfaced read-only
at `/proc/kvmm` through the procfs `vmm` feature (`procfs = { features =
["vmm"] }`). Per-vCPU counters live in `VcpuStats` (one cache-line slot per
vCPU, `Relaxed` atomics); the run loop stamps guest/host ticks around each entry
and the arch exit handler tags `vcpu.exit_category`.

Guest image loading and DTB patching live in `kvmm-api`, alongside the boot
control-plane policy that chooses image paths, memory slots, and virtual
platform wiring. `kvmm` exposes only the guest-memory mechanism that loader code
uses through `GuestMem`.

### Virtual device model and interrupt path (AArch64)

The shared device registry and cross-architecture device traits live under
`src/vdev/`. AArch64 guest-platform devices live under `src/vdev/aarch64/` and
are the sandbox used to bring up and debug the whole interrupt path with the
simplest possible guest:

- **`stage2` maps only guest RAM** (Normal). Every device region is left
  *invalid* so guest MMIO faults to EL2 — the guest can never touch the real
  host GIC/UART. The one hardware exception is added with a fine-grained L3
  mapping (`ensure_l3_table` splits a 2 MiB block into 4 KiB pages).
- **`vpl011`** emulates the PL011 UART; TX is line-buffered to the host log
  (`[guest N] …`), RX flows through an SPSC `RxChannel`.
- **`aarch64::vgicd`** emulates the GIC distributor (GICD, `0x0800_0000`). A byte-addressable
  backing store gives correct read-back for the registers the guest probes
  (IPRIORITYR/ITARGETSR/ICFGR/CTLR — without this the priority-bit probe reads 0
  and the guest hangs). Distributor writes update vGIC state: SGI/PPI enable bits
  are banked per vCPU, shared SPI enable bits are VM-wide, and ISPENDR/SGIR latch
  pending interrupts for the selected target vCPUs.
- **GICC → GICV**: the guest CPU interface (`0x0801_0000`) is redirected to the
  hardware virtual interface GICV (`0x0804_0000`) via `map_region`, so ack/EOI
  are hardware-assisted.
- **`aarch64::vgic`** owns virtual CPU-interface/GICH state. Pending interrupts are
  latched independently from enable state; `VgicHook::on_entry` drains only
  `pending & enabled` into free LRs inside the IRQ-masked world-switch window.
  Disabled pending lines stay latched until the guest enables them. LRs are read
  back in `on_exit`, then the physical GICH LR/VMCR/APR state is cleared so
  another VM that later runs on the same pCPU cannot observe stale virtual
  interrupt state.
- **Virtual timer**: guest `CNTV_CTL_EL02`/`CNTV_CVAL_EL02` are saved on exit and
  restored on entry; the timer is not disabled on exit because the host IRQ 27
  path must keep observing guest virtual timer expiry. The host IRQ route only
  wakes the per-pCPU owner task; before each guest entry, the VMM recomputes
  whether the current vCPU's saved virtual timer deadline is overdue and latches
  virtual PPI 27 pending when needed. Delivery still requires the guest's banked
  PPI 27 enable bit for that vCPU, matching GIC PPI semantics.
- **MMIO data path**: `handle_data_abort` decodes the ESR ISS (ISV/SAS/SRT/WnR)
  and moves the actual bytes between the faulting GPR and the device — a read
  writes the result back into the destination register, a write forwards the
  source register value.

End-to-end this boots FreeRTOS, delivers its timer tick, and runs the Rhealstone
benchmark.

### RISC-V validation devices

RISC-V guest-platform validation devices live under `src/vdev/riscv64/`. They
are intentionally smaller than a production virtual platform: the goal is to
exercise the same `VmDevices`/`IrqSender`/`VcpuHookFactory` abstraction used by
AArch64 while keeping the PLIC and timer model small enough for bring-up.

- **`riscv64::irq`** implements a simplified vPLIC at the QEMU virt PLIC window
  (`0x0c00_0000`, 4 MiB). It supports the PLIC priority, pending, enable,
  threshold, and claim/complete register layout for the first 64 sources, and
  maps PLIC context N directly to vCPU N. Claim moves a source from pending to
  active; completion clears active. The vCPU hook summarizes each context's
  deliverable source as VS external interrupt pending (`hvip.VSEIP`) and clears
  that per-pCPU CSR bit on exit so another VM cannot inherit stale virtual
  interrupt state.
- **`riscv64::timer`** raises VS timer interrupt pending (`hvip.VSTIP`)
  periodically while a vCPU is entered. This validates timer delivery through
  the hook path; a real implementation should replace it with SBI/Sstc deadline
  state programmed by the guest.
- **`arch::riscv64` CSR glue** delegates and enables VS external/timer interrupt
  injection (`hideleg`/`hie`) during per-CPU H-extension init, and exposes narrow
  helpers for the RISC-V vdev hooks to set or clear the current pCPU's `hvip`
  pending bits.

**Console UART (`ns16550a`, polled).** The emulated 16550A (`vdev-uart16550`)
drives the guest `ttyS0` in **polled mode**: the guest DTB `uart@10000000`
node deliberately carries *no* `interrupts`/`interrupt-parent`, so Linux binds
`ttyS0` without an IRQ and services it from its poll timer. Both directions
work without any interrupt:

- **TX** — the emulated transmit-holding register drains instantly, so `LSR`
  always reports THR-empty; the guest writes each byte and moves on. Output is
  line-buffered to the host log.
- **RX** — host→guest bytes enter through `VmDevices::push_console`, which
  pushes into the shared `RxChannel`. The guest's poll timer reads `LSR`, sees
  `DATA_READY`, and drains the byte from `RBR`. No injection is needed.

The UART *also* carries an optional interrupt path (`attach_irq` wires an
`IrqSender`; `push_console` injects PLIC source 10 when RX interrupts are
enabled; the THR-empty condition re-asserts source 10; `IIR` reports the
highest-priority cause). It is dormant while the DTB omits `interrupts`, and
exists so interrupt-mode `ttyS0` can be enabled later. It is intentionally
**not** enabled today: declaring `interrupts = <10>` flips Linux into
interrupt-driven mode, where TX blocks waiting for a THR-empty interrupt — and
the rv64 external-interrupt delivery path (`hvip.VSEIP`) has not yet been
validated end-to-end for this line, so the console TX stalled and even the
login prompt never printed. Polled mode avoids that dependency entirely.

**IRQ exit scheduling.** Kernel preemption is disabled here (cooperative
scheduling), so the vCPU thread must give up its pCPU voluntarily — but *how
often* matters a lot. Yielding on every IRQ exit throttled the guest badly
(host-timer preemptions via `HCR.IMO` fire ~100 Hz, and each yield let the run
loop ping-pong so the guest only ran ~49 % of the time); never yielding starved
host tasks that share the vCPU's pCPU (busybox stalled). The fix: the IRQ exit
handler just re-enters the guest (the host IRQ was already serviced at unmask),
and the run loop does a **bounded periodic yield** — at most once per ~1 ms of
wall time. When the vCPU is the only runnable task that yield is a cheap
self-repick (~µs, measured); when a host task is ready on the pCPU it hands the
CPU over. Result on the Rhealstone task-switch test: jitter 10.3 ms → 263 µs,
vCPU utilisation 49 % → 98 %, host shell responsive, vtimer inject latency
~130 µs. The remaining knob (a later step) is letting the vCPU actually sleep
during guest WFI instead of busy-yielding, and ultimately enabling kernel
preemption so no explicit yield is needed.

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

### 8. x86_64: Sporadic `assertion failed: !curr.is_idle()` after guest-mem selftest

**Symptom**: After `vmm_selftest_guest_mem` passes, the kernel sporadically panics with
`assertion failed: !curr.is_idle()` in the scheduler run queue. The vCPU thread's
`current_task_ptr()` returns an idle task from a different CPU.

**Reproduction**: Only triggers when `vmm_selftest_guest_mem` (unpinned vCPU thread) runs.
`vmm_selftest_smp` (CPU-pinned threads) never triggers it, even under heavy load.

**Root cause**: `vmcs_init_vcpu` writes `HostBaseGs = rdmsr(MSR_GS_BASE)` once at
initialization time, capturing the GS base of whatever CPU runs `init_vcpu`. On x86_64,
GS base points to the per-CPU data area — `current_task_ptr()` is a single `gs:[offset]`
load. The `selftest_guest_mem_impl` pattern is:

```
init_vcpu(...)          ← runs on CPU X, VMCS captures CPU X's GS base
spawn_vcpu_thread(vcpu) ← new thread scheduled on CPU Y
  → vmptrld + vmlaunch  ← executes on CPU Y
  → VM exit             ← processor restores HostBaseGs = CPU X's GS base
  → now running on CPU Y with CPU X's per-CPU pointer
  → current_task_ptr() returns CPU X's current task (possibly idle)
  → scheduler asserts !is_idle() → panic
```

The SMP selftest is immune because each thread pins itself with `set_current_affinity`
before calling `init_vcpu`, so init and execution always happen on the same CPU.

**Fix**: Refresh `HostBaseGs` in the VMCS before every `vmlaunch`/`vmresume`:

```rust
fn enter_guest(vcpu: &mut Vcpu<Self>) -> bool {
    vmptrld(vcpu.arch.vmcs_pa);
    vmcs_write(VmcsField::HostBaseGs, rdmsr(MSR_GS_BASE));  // ← fix
    vmx_enter_guest(...);
}
```

This ensures VM exit always restores the correct CPU's GS base, regardless of
thread migration between entries.

### 9. x86_64: VMCS cross-CPU migration without vmclear — hang in CI

**Symptom**: `vmm_selftest_guest_mem` hangs when run after `vmm_selftest_smp`. The
vCPU thread stops making progress at a random HLT iteration. Running `guest_mem`
alone (before `smp`) passes consistently.

**Reproduction**: `-smp 4 -cpu host -accel kvm` (nested VMX). The SMP test performs
VMXON on all 4 CPUs; the subsequent unpinned `guest_mem` thread can then be
rescheduled to any of them.

**Root cause**: Intel SDM Vol 3C §24.11.2 requires that a VMCS be vmclear'd on its
current logical processor before being loaded (vmptrld) on a different one. Without
vmclear, the destination CPU may read stale processor-internal VMCS cache — producing
undefined behavior (typically an infinite hang on vmlaunch/vmresume).

The `handle_hlt` exit calls `ktask::yield_now()`, which allows the scheduler to
migrate the vCPU thread to a different physical CPU. On resume, `enter_guest` does
`vmptrld` on the new CPU without the required vmclear on the old CPU.

Additionally, even if vmclear is performed, the per-CPU host state fields in the VMCS
(HostBaseGdtr, HostBaseTr, HostBaseGs, HostBaseFs, HostCr3) are stale — they describe
the old CPU's GDT, TSS, and per-CPU area. VM exit restores these stale values into
the host, corrupting scheduler state on the new CPU.

**Fix**: Two-part fix following Intel SDM §24.11.2:

1. **vmclear before yield** in `handle_hlt`:
```rust
fn handle_hlt(vcpu: &mut Vcpu<X86Vmx>) -> ExitAction {
    vmcs_write(VmcsField::GuestRip, rip + inst_len);
    vmx::vmclear(vcpu.arch.vmcs_pa);  // flush VMCS to memory
    vcpu.launched = false;             // next entry must use vmlaunch
    ktask::yield_now();                // thread may migrate
    ExitAction::Resume
}
```

2. **Refresh all per-CPU host state** in `enter_guest` after vmptrld:
```rust
pub fn refresh_host_state() {
    let gdt_base = sgdt_base();
    vmcs_write(VmcsField::HostCr3, read_cr3());
    vmcs_write(VmcsField::HostBaseGdtr, gdt_base);
    vmcs_write(VmcsField::HostBaseIdtr, sidt_base());
    vmcs_write(VmcsField::HostBaseTr, read_tss_base(gdt_base));
    vmcs_write(VmcsField::HostBaseFs, rdmsr(MSR_FS_BASE));
    vmcs_write(VmcsField::HostBaseGs, rdmsr(MSR_GS_BASE));
}
```

This supersedes the earlier fix (#8) that only refreshed HostBaseGs — the full
refresh handles all per-CPU-variable host fields.

---

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

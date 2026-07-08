# kvmm — Interrupt Routing & Preemption Race Fixes

## Problem

When a guest executes a tight loop (no WFI/HLT/VMCALL), the VMM must rely
on hardware timer interrupts to force a VM exit and yield CPU time to the
host scheduler. Without proper interrupt routing and preemption protection,
multi-VM workloads hang or corrupt state.

---

## x86 VMX

### 1. Missing external-interrupt exiting

Pin-based VM-execution controls lacked bit 0 (`external-interrupt exiting`).
Without it, hardware interrupts during guest execution do not cause a VM exit;
the CPU remains in VMX non-root mode indefinitely.

**Fix**: Set `PIN_EXTERNAL_INT_EXIT` (bit 0); handle exit reason 1 with
`yield_now()` + resume.

### 2. VMCS preemption race (reason=33, rip=0x0)

Between `vmptrld` and `vmlaunch`/`vmresume`, if the thread is preempted,
another thread's `vmptrld` steals the "current VMCS" on that logical CPU.
The original thread's `vmlaunch` then uses the wrong VMCS → entry failure.

Same race exists in `vmcs_init_vcpu` (vmptrld → vmwrites → vmclear).

**Fix**: `cli`/`sti` brackets around the critical sections.

### 3. Overly broad cli in vmcs\_init\_vcpu

The initial fix wrapped ~150 lines including `build_guest_identity_pt()`
(allocates 6 pages) inside the `cli` window, degrading scheduling latency.

**Fix**: Pre-compute all values (rdmsr, CR reads, page table allocation)
before `cli`. Only the pure `vmwrite` sequence runs with interrupts disabled.

---

## AArch64 VHE

### 1. Missing HCR\_EL2.IMO/FMO

Guest entry cleared TGE and set TWI but did not set IMO (bit 4) or FMO (bit 3).
Physical IRQs were delivered to EL1 (guest) instead of trapping to EL2 (host).
Guest has no IRQ handler → system hangs.

**Fix**: Set IMO+FMO on guest entry; clear them on guest exit.

### 2. Stale ESR\_EL2 after async exits

ESR\_EL2 is only updated for synchronous exceptions. IRQ/FIQ/SError exits
leave ESR stale. If the previous exit set EC=0x01 (WFI), an IRQ exit would
incorrectly invoke `handle_wfi()` → `advance_pc()` corrupts guest PC.

All 16 vector table entries branched to the same `el2_trap_exit` with no
way to distinguish synchronous vs asynchronous exceptions.

**Fix**: Each vector entry writes an exit type (0=sync, 1=IRQ, 2=FIQ,
3=SError) to `vcpu->exit_type` (offset 520) via `tpidr_el2` before
jumping to the common save path. `exit_handler` checks `exit_type` first;
non-zero → yield + resume without reading ESR.

### 3. tpidr\_el2 preemption race (EC=0x25, FAR=low address)

With VHE (E2H=1), both `tpidr_el1` and `tpidr_el2` encodings access the
same physical register (TPIDR\_EL2). If a vCPU thread is preempted between
`msr tpidr_el2, vcpu*` and `eret`, another vCPU thread on the same CPU
overwrites the register. On resume, the original thread's guest traps and
the vector entry reads a stale/wrong pointer → EL2 data abort (EC=0x25)
at a low address.

**Fix**: `msr daifset, #2` (mask IRQ) at the top of `el2_enter_guest`,
before writing `tpidr_el2`. The eret restores guest PSTATE from SPSR\_EL2.
After trap exit, `exit_handler` unmasks with `msr daifclr, #2`.

---

## RISC-V H-extension

### Timer interrupt exit without yield

`hideleg=0` (default) ensures supervisor timer interrupts trap from VS-mode
to HS-mode. However, the `is_interrupt` branch in `exit_handler` only
toggled `sstatus.SIE` to service the interrupt, then immediately resumed
the guest without yielding — the scheduler never got a chance to run.

**Fix**: Add `ktask::yield_now()` after servicing the interrupt.

---

## Design Rules for Future Arch Backends

1. **Timer must force VM exit** — hardware timer interrupts must be routed
   to the hypervisor while the guest is executing (x86: pin-ctl bit 0;
   aarch64: HCR.IMO; riscv: don't set hideleg for STI).

2. **Yield after async exits** — the exit handler must call `yield_now()`
   (or `sleep`) to give the scheduler an opportunity to run other threads.

3. **Atomic guest-entry window** — critical register writes (VMCS current,
   tpidr_el2, HCR mode switch) must be protected from preemption (cli/sti,
   daifset/daifclr) until guest entry completes.

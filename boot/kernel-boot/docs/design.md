# Boot Entry Design Notes

## Ordinary IRQ Exclusion

RISC-V and LoongArch primary and secondary entries establish ordinary IRQ
exclusion before stack setup, trap installation and controller preparation.
RISC-V clears only SSTATUS.SIE; LoongArch exchanges only CRMD.IE with zero.
These operations need neither a current task nor per-CPU storage. Individual
source enables, pending interrupts and unrelated status fields are preserved.
Runtime owns the later enable after handlers and local services are ready.

The LoongArch primary sequence begins after the fixed 64-byte boot header;
its load offset and entry address are unchanged. The scratch registers used
for the exchange are not firmware argument registers.

Linux also establishes early exclusion, but the mechanisms are not identical:
`arch/riscv/kernel/head.S` clears CSR_IE/CSR_IP at primary and SBI-secondary
entry, while `arch/loongarch/kernel/head.S` invokes `SETUP_MODES`, which writes
CRMD with IE clear. X-Kernel preserves source and pending state and does not
copy Linux's complete mode setup. This does not add RISC-V M-mode boot support.

Validation checks the emitted entry instructions and boots the corresponding
QEMU configurations. Normal firmware boot alone does not prove behavior with
deliberately enabled incoming IRQs; no such firmware fault injection is claimed.
See [the entry validation procedure](irq-entry-validation.md) for commands and
the primary/AP coverage boundary.

## AArch64 EL0 Cache Access

### Scope and ownership

This note covers the AArch64 boot MMU's user cache-access policy.
`entry.rs` owns the primary and secondary entry sequences, `el.rs` selects
the exception-level setup, and `mmu.rs::init_mmu` programs each CPU's
translation and control registers before handing execution to the runtime.

### Execution context and ordering

Both entry paths initialize the exception level and FP/SIMD state before
calling the shared `init_mmu` in early privileged boot.
The primary prepares the boot page tables before releasing secondary CPUs.
Each CPU programs its own registers using these already initialized tables;
this policy introduces no shared mutation, allocation, lock or cleanup path.
The existing page-table, executing-code and stack-mapping preconditions of
`init_mmu` continue to apply.

### User cache-access policy

The read-modify-write that enables translation and caching stays unconditional;
a following modify guarded by `#[cfg(not(feature = "vmm"))]` sets
`UCT::DontTrap`, `UCI::DontTrap` and `DZE::DontTrap`.
In the ordinary EL1/EL0 regime, these permit an EL0 `CTR_EL0` read, remove the
SCTLR trap on cache-maintenance instructions such as `DC CVAU` and `IC IVAU`,
and allow `DC ZVA` cache-line zeroing probed by vectorized memset and memcpy.
These operations are used by user runtimes to publish generated code.
Relying on firmware/reset values can instead turn valid operations into SIGILL.

The common per-CPU location gives primary and secondary CPUs the same policy.
The VMM build keeps its existing policy: under VHE
(`HCR_EL2.{E2H,TGE}={1,1}`) the same-named controls alias into `SCTLR_EL2`,
and that regime is outside this audit.
The `vmm` cfg is a build-time premise: those builds boot at EL2/VHE via
`el::init_el2_vhe`; when such a build falls back to EL1 (`CurrentEL < 2`),
the grant is compiled out and the reset policy remains — a pre-existing gap
left to a future runtime regime check, not addressed here.
Unrelated SCTLR fields and the existing post-write ISB remain unchanged.
Linux's [initial SCTLR configuration](https://github.com/torvalds/linux/blob/v6.12/arch/arm64/include/asm/sysreg.h#L824)
also includes DZE/UCT/UCI; LLVM's [AArch64 clear-cache implementation](https://github.com/llvm/llvm-project/blob/llvmorg-20.1.0/compiler-rt/lib/builtins/clear_cache.c)
uses the corresponding instructions.

### Limits and validation

The regression targets ordinary EL1, homogeneous AArch64 QEMU CPUs.
It checks user-visible instruction success and a small generated-code sequence,
with a genuine undefined-instruction control.
It does not prove physical cache coherency, cross-CPU code publication or VHE behavior.
Heterogeneous CPUs may need a sanitized system-wide CTR value and an appropriate
trap/emulation policy; direct reads alone do not establish that contract.

See [the standalone regression](../../../docs/experiments/aarch64-el0-cache/README.md)
for its build and execution procedure.

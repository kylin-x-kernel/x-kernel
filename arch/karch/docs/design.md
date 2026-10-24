# Architecture Primitives

## AArch64 PMR Bring-up

After successful GICv3 CPU-interface preparation, `pmr::initialize_masked`
sets `NMI_ONLY` before publishing per-CPU readiness. DAIF.I remains set across
this transition. Controller initialization guards have already been released,
so they cannot restore a pre-transition token during this operation. Later
IRQ helpers may clear DAIF.I while PMR still blocks ordinary IRQs; explicit
IRQ enablement remains the runtime's responsibility.

Linux arm64's `init_gic_priority_masking` and `local_daif_restore` likewise
establish PMR state before relying on priority masking. X-Kernel uses its own
mask values and readiness flag, not Linux's PSR-I-in-PMR encoding. This is a
boot transition, not an idempotent runtime readiness query.

## Idle Wait

`idle_wait` is the scheduler's IRQ-off wait primitive, distinct from the
terminal `stop_cpu` path. The caller disables preemption
and normal IRQs, checks retained work, and holds no scheduler lock across the
wait. The primitive may return spuriously and always returns IRQ-off. It does
not decide whether work is pending or perform scheduler/accounting operations.

| Architecture | Wake mechanism |
| --- | --- |
| x86_64 | Adjacent `sti; hlt`, followed by `cli`; the STI shadow prevents a normal IRQ from being consumed before HLT. |
| RISC-V | Memory fence and WFI with SIE still clear; individual interrupt-source enables remain unchanged. |
| AArch64 | DSB and WFI with CPU IRQ delivery masked. In PMR mode, mask DAIF.I/F before opening PMR, and restore PMR before DAIF; hardware-NMI ALLINT is unchanged. |
| LoongArch64 | A leaf enables IE before IDLE and disables it on return. Kernel IRQ entry redirects a saved PC pointing at IDLE to its exit, preventing sleep after a wake was already handled. |

Normal IRQ handlers may execute inside the x86/LoongArch primitive. The caller
must retain preemption exclusion until its post-wait bookkeeping is complete.
This does not require keeping IRQs masked throughout bookkeeping. Host x86
builds only provide a spin fallback and do not model the IRQ-off contract.
Wake sources must remain enabled at the controller; masking interrupt delivery
at the CPU is not equivalent to suppressing the wake signal at the controller.

For AArch64, Arm's [Critical Interrupt Prioritization](https://documentation-service.arm.com/static/5ed6547eca06a95ce53f9321),
page 7, "Additional considerations for idle and power-down code", explains that
PSTATE-masked pending interrupts can wake WFI, whereas interrupts blocked by the
GIC priority mask cannot. Hence PMR mode masks CPU exception delivery before
opening the controller priority mask. Linux follows this split in
`arch/arm64/kernel/idle.c::cpu_do_idle` and
`arch/arm64/include/asm/cpuidle.h::arm_cpuidle_save_irq_context`.

Waking from WFI does not require executing the IRQ handler inside `idle_wait`:
on AArch64 the pending IRQ can be serviced after the caller restores IRQ
delivery. Tests therefore observe notification handling across that restoration,
while retaining an outer no-preempt guard until the observation is complete.

## Cache Isolation Layout

### Scope

The following sections cover `src/cache.rs` and `src/<arch>/cache_layout.rs`, the pure
layout part of `karch`.
Architecture-specific instruction operations remain described in the crate
README and their source rustdoc; this is not a redesign of cache maintenance.

### Contract and Implementation

`CacheAligned<T>` owns one `T`, uses architecture-selected `repr(align(...))`, and exposes ordinary
shared/mutable borrows through `Deref`/`DerefMut`. `new` is const and does not
allocate, run privileged instructions or depend on CPU/runtime initialization.
Normal Rust moves and destruction apply; this is not pinned storage.

Each architecture owns a `src/<arch>/cache_layout.rs` file. It invokes a private
macro with its alignment literal; the common module selects that file by
`target_arch` and supplies the shared API implementation. There is no generic
fallback for an unknown architecture. `SMP_CACHE_BYTES` derives from the selected
`CacheAligned<()>`, keeping the numeric policy and representation consistent.

| Architecture | Layout policy | Basis |
| --- | ---: | --- |
| AArch64 | 64 bytes | Linux arm64 generic L1/SMP layout, distinct from DMA alignment |
| x86_64 | 64 bytes | Linux X86_64 default; legacy x86-32 CPU choices are not applied |
| RISC-V32/64 | 64 bytes | Linux generic RISC-V layout |
| LoongArch64 | 64 bytes | Linux default L1_CACHE_SHIFT=6 |
| ARM32 | 128 bytes | Conservative coverage of Linux's 32/64/128-byte ARM configuration choices |

The source comments link to the Linux definitions behind each choice. These
are architecture-owned build-time policies, not claims that every CPU of an
ISA has the same hardware line size. A platform needing a different policy must
extend its architecture's selection rather than changing the common wrapper.
UP builds use the same policy. Stronger alignment required by `T` is preserved;
nonempty arrays have padded strides. Zero-sized values need not have distinct
addresses. This policy is not detected cache geometry, DMA alignment or an
instruction stride, and may not isolate larger hardware coherence lines.

### Consumers and Validation

Timer CPU slots wrap the complete spinlock and protected queue. Watchdog wraps
its per-CPU heartbeat state while preserving its public type and methods.
Neither caller gains synchronization from padding; their existing concurrency
protocols remain authoritative. The host timer harness imports this exact pure
source file, rather than linking the kernel configuration dependencies of
`karch`. Tests cover array isolation, larger payloads, stronger alignment and
value access; consumer tests cover timer slots and watchdog state.

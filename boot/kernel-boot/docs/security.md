# Boot Entry Security Notes

## Ordinary IRQ Entry Boundary

The RISC-V and LoongArch entry instructions explicitly mask ordinary IRQ
delivery before using a boot stack or calling Rust. They rely on privileged
execution and the existing firmware/linker entry contract, not scheduler guards.
Only SSTATUS.SIE or CRMD.IE is cleared; paging, privilege level, pending sources
and firmware argument registers are preserved. LoongArch's primary boot header
must remain exactly 64 bytes before the first executable entry instruction.

This is not NMI, machine-level interrupt or synchronous-exception suppression.
It does not make an otherwise invalid firmware handoff safe. Runtime must keep
ordinary delivery excluded through controller preparation and handler setup;
these entry writes do not protect a later premature enable.

## AArch64 EL0 Cache-Access Boundary

### Trust and unsafe boundary

`mmu.rs::init_mmu` trusts privileged entry state, linker addresses and initialized
boot page tables that preserve its executing code, stack and required data.
The primary and secondary assembly callers establish the exception-level and
boot-stack prerequisites before invoking it.
The UCT/UCI/DZE policy uses the existing typed register read-modify-write API
and adds no raw pointer access, assembly block or unsafe operation.
Register state is CPU-local; the existing ISB synchronizes the control change.

### Access granted

EL0 can read the implemented cache description and execute the cache operations
governed by these controls, including `DC ZVA` zeroing of one cache line.
`DC ZVA` remains an ordinary data access: it cannot bypass translation or
page-permission checks, and `DCZID_EL0` still reports the capability truthfully.
Page-table permissions, exception classification and other architectural trap
controls remain separate and unchanged.
This does not enable all privileged instructions or create executable mappings.
User code must still follow the required translation, cache and barrier rules.

Cache parameters become visible to userspace, and cache maintenance can affect
timing and contention; the change makes no side-channel isolation guarantee.
Heterogeneous systems need a separate policy for a safe common cache description.
The VHE path aliases control-register accesses and is outside the demonstrated
non-VMM regression scope; it must not be described as unaffected or validated.
The `vmm` build gate is a compile-time premise: a vmm build that boots below
EL2 keeps the reset policy for these controls — the pre-change state, not a
newly audited configuration.

### Failure analysis and review

| Failure | Cause and effect | Control or limitation |
|---|---|---|
| Valid user cache operations fault | Reset permissions disable CTR/cache access, causing SIGILL | Set named UCT/UCI fields explicitly and execute the instructions from EL0. |
| Behavior depends on CPU placement | Only the primary CPU is configured | Keep the update in the shared primary/secondary boot path. |
| Boot control ordering regresses | Unrelated SCTLR bits or the ISB are changed | Preserve read-modify-write and the original enable sequence. |
| Fault handling is accidentally masked | A workaround suppresses illegal-instruction delivery | Retain a genuine undefined-instruction negative control. |

This AArch64 section does not audit the crate's other boot paths.
The cache-access change introduces no runtime resource or diagnostic data collection.

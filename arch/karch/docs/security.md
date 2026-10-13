# Architecture Primitive Safety

## Idle Wait Boundary

On bare metal, the unsafe `idle_wait` contract requires IRQ/preemption exclusion, initialized
wake sources, task-context NMI state and no locks required by IRQ handlers.
Violating it can deadlock a CPU or permit scheduling during a caller's excluded
region. The operation allocates nothing and accepts no external data. Debug
assertions check normal IRQ masking; callers own the remaining obligations.
The x86 host fallback only spins once; it neither changes IRQ state nor validates
the bare-metal masking contract.

The AArch64 PMR sequence is a bounded exception to the ordinary DAIF invariant:
no ordinary IRQ helper runs between CPU masking and exact mask restoration.
Hardware-NMI ALLINT is preserved. LoongArch IRQ entry fixes up only kernel
frames whose PC exactly names the dedicated IDLE instruction; no user-supplied
PC is rewritten. The assembly leaf preserves callee-saved registers and returns
with IE clear. Other architectures also return IRQ-off, including spurious wakes.

These guarantees prevent consuming a normal wake before an unconditional sleep;
they do not guarantee controller delivery, support CPU hotplug or change terminal
CPU-stop behavior. Disassembly checks and real IRQ tests complement caller review.

## Cache Isolation Safety

### Scope and Inputs

This analysis covers `src/cache.rs` and `src/<arch>/cache_layout.rs` only.
Privileged architecture operations,
cache-maintenance instructions and their callers are outside this layout-only
analysis and retain their existing source-level safety contracts.

The sole input is an owned Rust value `T`. This module has no raw-pointer, FFI,
user-memory, device, file or network parsing entry point and no unsafe code.
It performs no allocation, I/O, logging or synchronization.

### Invariants and Limitations

Rust enforces alignment, initialization, borrowing and normal destruction.
`Deref` and `DerefMut` borrow the contained field; no pointer conversion or
ownership duplication is involved. Send/Sync follow `T` automatically. Padding
is not initialized application data and must not be serialized or read as such.

The wrapper cannot prevent races within `T` or replace locks/atomics. It does
not guarantee a stable address, a DMA-safe buffer or correct cache-maintenance
coverage. A coherence line larger than the compile-time policy can still cause
false sharing; the residual effect is performance loss, not a relaxed safety
contract. Its public methods have no explicit panic path; dropping `T` retains
the payload's normal destructor behavior. It stores no additional user data.

Architecture policies are trusted build inputs, selected by `target_arch`.
The private definition macro uses one alignment literal per architecture and
the public constant derives from the selected type, preventing inconsistent
numeric and layout definitions. Unknown architectures are rejected at compile
time instead of receiving an unverified fallback.

### Threat Model and Responses

The protected assets are payload memory safety and confidentiality, the integrity
of the selected layout policy, and the availability of consumers using aligned
storage. The relevant boundaries are architecture policy selection at build time
and kernel callers wrapping, borrowing, or interpreting `CacheAligned<T>`.
There is no direct untrusted runtime input parser. The risks below include
integration mistakes; they do not imply that safe use of this wrapper permits UB.

Severity: High means memory corruption, UB or disclosure; Medium means loss of
service; Low means performance degradation. Response labels distinguish existing
controls from responsibilities that remain with consumers.

| ID | Asset / boundary | Trigger and impact | Severity | Response strategy, implemented controls and residual risk |
| --- | --- | --- | --- | --- |
| T01 | Layout policy / architecture selection | A policy smaller than the processor's coherence line, or wrapping only part of a shared synchronization unit, leaves false sharing. Contending CPUs incur extra coherence traffic and latency. | Low | **Mitigate and accept residual risk.** Architecture-owned literals generate the wrapper; `SMP_CACHE_BYTES` derives from its actual alignment. Unknown architectures fail compilation. Array-stride and stronger-alignment tests check the declared policy, not hardware geometry. Callers must wrap the complete independent object. Larger hardware lines can still share storage; platform validation remains necessary. |
| T02 | Payload integrity / shared access and lifetime | A caller treats alignment as synchronization or pinning and uses unsafe access to bypass locks or moves a published intrusive object. Data races or stale pointers can corrupt memory. | High | **Mitigate; retain caller responsibility.** `Deref`/`DerefMut` use ordinary Rust references and Send/Sync follow `T`; the module adds no unsafe access or lifetime extension. Safe code remains subject to borrowing and trait checks. Consumers must preserve their existing locks, atomics, pinning and teardown protocols; the wrapper cannot validate external unsafe code. |
| T03 | Payload confidentiality and validity / raw representation | External unsafe code reads or exports the wrapper's complete byte representation, including padding, as initialized data. This can cause UB or disclose stale memory. | High | **Mitigate; retain caller responsibility.** The API exposes the contained value, not a raw byte view or serialization method. Padding is explicitly excluded from initialized application data. Consumers must serialize initialized fields rather than copying the full object representation; no padding-zeroing guarantee is made. |
| T04 | Memory/device data integrity / hardware consumers | A caller uses `SMP_CACHE_BYTES` as a cache-maintenance instruction stride or assumes the wrapper establishes DMA suitability. Incorrect coverage or buffer assumptions can produce stale or corrupted data. | High | **Mitigate and transfer validation to the hardware integration boundary.** Module rustdoc explicitly excludes both contracts, and the wrapper performs no hardware operations. Architecture maintenance APIs and DMA providers must establish their own geometry, mapping and ownership requirements. This analysis does not claim to audit those excluded implementations. |
| T05 | Consumer availability / storage footprint | Applying padded alignment to a large array increases static, stack or heap usage, potentially exceeding a memory budget. Stack exhaustion or allocation failure can interrupt service. | Medium | **Mitigate at the consumer boundary.** Construction itself allocates nothing and Rust exposes the real padded size through `size_of`; no hidden allocation or fallback is introduced. Consumer review must check array size and placement against its memory budget. The module imposes no quota or recoverable allocation policy on its owners. |

### Failure Modes and Verification

| ID | Failure mode and cause | Local / system effect | Control and verification |
| --- | --- | --- | --- |
| F01 | Missing architecture policy | Build fails; no silently guessed layout reaches a kernel image. | Explicit `compile_error!` for unsupported architectures; compile each supported target when policy selection changes. |
| F02 | Isolation policy does not match the consumer or hardware | Safe layout may still suffer false sharing and higher latency. | Array-address tests establish policy-sized separation, including larger payloads; consumer/platform measurements are needed to assess actual coherence behavior. |
| F03 | Excessive padded storage | Consumer footprint grows and may exceed its storage budget. | Review `size_of` and element count at each adoption; allocation and stack limits remain consumer responsibilities. |

The tests in `src/cache.rs` also check that payloads with stronger alignment retain
it and that shared/mutable value access remains correct. These are layout checks,
not evidence of DMA safety, synchronization, pinning or hardware cache behavior.

### Audit Checklist

- Wrap the complete independently updated object, including its lock.
- Preserve existing ownership, pinning and synchronization protocols.
- Check padding cost before applying the wrapper to large arrays.
- Keep the host harness and production consumers on the same implementation.
- Do not reuse the layout constant as a hardware cache instruction stride.

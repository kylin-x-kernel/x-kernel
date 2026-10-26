# Unsafety

Use this file for any change that adds,
modifies, or reviews `unsafe` code.

## Audit Surface

- encapsulate unsafe state in the smallest practical module;
- if private fields participate in a safety invariant,
  any code that can mutate them is part of the same audit surface;
- when multiple unsafe paths share one invariant,
  review them together rather than independently.

## Preferred Techniques

- prefer expressing unsafe invariants in types and API shape
  rather than relying only on comments or caller discipline;
- when a plain `T` cannot legally represent the state,
  introduce a type that does, such as `MaybeUninit<T>`,
  `NonNull<T>`, pin wrappers, newtypes for raw handles,
  or guard/token types for validated access;
- use types to carry as much of the safety story as possible;

## Governance Workflow

Treat unsafe cleanup as a standards-driven process,
not an open-ended search for smaller `unsafe` counts.

For each unsafe site:

1. classify the site into one primary scenario from the catalog below;
2. check whether the repository already has a matching abstraction;
3. prefer moving the site behind that abstraction
   over inventing a one-off local wrapper;
4. if no abstraction exists, state that explicitly in review;
5. only then decide whether to keep the unsafe block
   with clearer invariants;
6. or introduce a new reusable abstraction;
7. or refactor the call site to an existing safe API.

If a recurring unsafe pattern is not covered by this skill,
the review should say so explicitly and propose
the missing standard before large-scale cleanup continues.

When extending this skill with a new unsafe scenario,
prefer grounding the new rule in this source order:

- official Rust documentation first,
  especially `std` / `core` docs,
  the Rust Reference,
  and the Rustonomicon;
- then project-relevant primary guidance such as
  Rust-for-Linux or Linux kernel Rust coding guidelines;
- then mature Rust projects whose abstractions are widely used
  and whose unsafe boundary matches the repository's needs.

Do not add a new scenario as pure local taste.
State which source established the recommended invariant,
abstraction shape, or review rule.

Unsafe reduction goals, in preferred order:

- remove real UB or latent invariant breaks;
- encode caller obligations in types;
- improve `SAFETY:` arguments and `# Safety` contracts.

Do not treat “fewer `unsafe` tokens” as success by itself.
Moving raw memory operations into a “safe” helper that merely hides
the same preconditions is not a governance win.

## Scenario Catalog

Use the first matching scenario.
The review should name the scenario explicitly.

### User Memory And Syscall ABI

For syscall access placement and validation, read
[CG-019](../../../code-guidelines/CG-019-user-pointers-stop-at-syscall-boundary.md);
for the type and copy contracts, read
the pending Dylint lint `unsound_user_abi_impl` ([pending](../../../code-guidelines/pending.md)) and
the pending Dylint lint `user_copy_without_abi_contract` ([pending](../../../code-guidelines/pending.md)).
Repository examples: [typed pointers](../../../../posix/types/src/ptr.rs)
and [checked access](../../../../core/kuaccess/src/lib.rs).

### Uninitialized Storage

For slots, arrays, and field-by-field initialization, read
[CG-018](../../../code-guidelines/CG-018-uninitialized-storage-model.md),
including its initialization-state checks, repository examples, and standard
references. This scenario applies before shared-state or ownership review
when storage does not yet contain a valid value.

### Shared Mutable State

Prefer:

- `SpinLock`, `RwLock`, or other lock types that centralize
  `UnsafeCell` access and expose guard-based references;
- atomics when the shared state is actually atomic
  and the ordering is part of the invariant;
- per-CPU wrappers or current-CPU confinement
  when exclusivity comes from execution context.

Avoid:

- exposing raw `UnsafeCell` pointers outside the owning abstraction;
- claiming `UnsafeCell` alone solves concurrency.

Repository examples:

- [task/kspin/src/lock.rs](/home/laoyekang/code/x-kernel/task/kspin/src/lock.rs:39)
- [task/ktask/src/task.rs](/home/laoyekang/code/x-kernel/task/ktask/src/task.rs:94)

Relevant standard references:

- `UnsafeCell<T>` only opts out of shared-reference immutability;
  it does not permit aliasing `&mut T` and does not prevent data races. See
  <https://doc.rust-lang.org/std/cell/struct.UnsafeCell.html>.
- Rust’s aliasing model is what lets the compiler optimize around `&mut`;
  unsafe code must not violate that model accidentally. See
  <https://doc.rust-lang.org/nomicon/aliasing.html>.

### One-Time Initialization And Lazy Globals

Avoid:

- partially synchronized “init if null” patterns;

Repository examples:

- [util/klazy/src/once.rs](/home/laoyekang/code/x-kernel/util/klazy/src/once.rs:43)
- [util/klazy/src/lazy.rs](/home/laoyekang/code/x-kernel/util/klazy/src/lazy.rs:23)

### Raw Ownership Pointers

Prefer:

- `NonNull<T>` when null is impossible and “present but maybe dangling”
  is a meaningful state boundary;
- an owner handle plus `NonNull<T>` when ownership/lifetime are coupled;
- plain raw pointers when nullability or variance is semantically required.

Avoid:

- `NonNull<T>` as a cosmetic replacement for every `*mut T`;
- using `NonNull<T>` in covariant positions when the abstraction
  actually mutates `T` and therefore needs invariance.

Repository examples:

- [drivers/contracts/net/src/net_buf.rs](/home/laoyekang/code/x-kernel/drivers/contracts/net/src/net_buf.rs:16)
- [mm/kalloc/src/lib.rs](/home/laoyekang/code/x-kernel/mm/kalloc/src/lib.rs:256)

Relevant standard references:

- `NonNull<T>` is non-null and covariant; if the abstraction mutates `T`
  through the pointer and variance matters, add an invariant marker such as
  `PhantomData<Cell<T>>`. See
  <https://doc.rust-lang.org/std/ptr/struct.NonNull.html>.

### Raw Byte Parsing And Unaligned Data

Prefer:

- by-value decoding via `read_unaligned` after bounds checks;
- `repr(C)`/layout-proven ABI structs for external binary formats;
- byte-slice parsing helpers that do not create references into possibly
  unaligned or partially initialized storage.

Avoid:

- creating `&T` from raw bytes unless alignment, initialization,
  and lifetime are all proven;
- joining contiguous slices across allocations and treating them as one slice.

Repository examples:

- [boot/firmware-handoff/src/efi.rs](/home/laoyekang/code/x-kernel/boot/firmware-handoff/src/efi.rs:398)
- [boot/kernel-elf-loader/src/lib.rs](/home/laoyekang/code/x-kernel/boot/kernel-elf-loader/src/lib.rs:102)

Relevant standard references:

- `read_unaligned` still requires readable, initialized data of type `T`;
  it only relaxes alignment. See
  <https://doc.rust-lang.org/std/ptr/fn.read_unaligned.html>.
- `slice::from_raw_parts` additionally requires one allocation,
  non-null/aligned pointers even for length 0, initialized elements,
  and no mutation through aliased shared references. See
  <https://doc.rust-lang.org/std/slice/fn.from_raw_parts.html>.

### Fixed-Capacity Raw Output Buffers

Repository examples:

- [boot/x86_64-uefi-loader/src/multiboot.rs](/home/laoyekang/code/x-kernel/boot/x86_64-uefi-loader/src/multiboot.rs:12)
- [boot/kernel-elf-loader/src/lib.rs](/home/laoyekang/code/x-kernel/boot/kernel-elf-loader/src/lib.rs:133)

Relevant standard references:

- `write_unaligned` still requires the destination pointer
  to be valid for writes; it only relaxes alignment. See
  <https://doc.rust-lang.org/std/ptr/fn.write_unaligned.html>.
- `write_bytes` requires the destination region to be valid
  for the full written byte count and properly aligned. See
  <https://doc.rust-lang.org/std/ptr/fn.write_bytes.html>.

### MMIO, Port I/O, And Volatile Access

Prefer:

- volatile reads/writes only for externally observable device memory semantics.

Avoid:

- using volatile accesses as a substitute for synchronization;
- mixing ordinary references with MMIO register semantics casually;

Repository examples:

- [drivers/arm-gic/src/gicv3.rs](/home/laoyekang/code/x-kernel/drivers/arm-gic/src/gicv3.rs:121)
- [drivers/platform/console/src/ns16550_mmio.rs](/home/laoyekang/code/x-kernel/drivers/platform/console/src/ns16550_mmio.rs:1)

Relevant standard references:

- volatile accesses are externally observable and are appropriate for I/O memory,
  but they are not atomic synchronization primitives. See
  <https://doc.rust-lang.org/std/ptr/fn.write_volatile.html>.

### FFI And Foreign Ownership

Prefer:

- a narrow `unsafe extern` declaration layer;
- `repr(C)` structs and explicit ABI newtypes at the boundary;
- explicit ownership and unwind policy in the wrapper docs.

Avoid:

- assuming foreign pointers are valid “because C gave them to us”;

Relevant standard references:

- Linux’s Rust coding guidelines are broadly abstraction-first
  and kernel-facing rather than “open-code unsafe everywhere.” See
  <https://docs.kernel.org/rust/coding-guidelines.html>.

### Boot Entry, Naked Functions, And CPU State Handoff

Prefer:

- a narrow boundary where raw boot entry, linker-defined symbols,
  naked assembly, and CPU-mode transitions are localized;
- explicit `# Safety` docs stating entry-state assumptions:
  calling convention, register/stack state, linker layout,
  MMU/exception-level state, and control-transfer behavior;
- safe helpers beneath the entry boundary once invariants
  have been materialized into typed values.

Avoid:

- mixing ordinary initialization logic into naked-entry or
  CPU-transition blocks when it can run after the unsafe handoff.

Repository examples:

- [boot/kernel-boot/src/arch/aarch64/entry.rs](/home/laoyekang/code/x-kernel/boot/kernel-boot/src/arch/aarch64/entry.rs:48)
- [boot/kernel-boot/src/arch/riscv64/entry.rs](/home/laoyekang/code/x-kernel/boot/kernel-boot/src/arch/riscv64/entry.rs:38)
- [boot/x86_64-boot-stub/src/main.rs](/home/laoyekang/code/x-kernel/boot/x86_64-boot-stub/src/main.rs:75)

Relevant standard references:

- the Rust Reference states that `#[unsafe(naked)]`
  carries the obligation to respect the function calling convention,
  uphold the signature, and return or diverge rather than fall through. See
  <https://doc.rust-lang.org/reference/attributes/codegen.html>.
- the Rust Reference further specifies that `naked_asm!`
  constitutes the full function body and documents the rules
  for register state, callee-saved registers, memory access,
  and control flow to avoid UB. See
  <https://doc.rust-lang.org/reference/inline-assembly.html>.

### Address-Sensitive And Self-Referential State

Prefer:

- `Pin`, `Unpin`, and `PhantomPinned`
  when soundness depends on stable address;
- explicit projection policy if fields become structurally pinned.

Avoid:

- relying on “this value probably will not move”;
- exposing `&mut` access that lets safe code move a pinned field out.

Relevant standard references:

- pinning is for values whose soundness depends on stable address,
  including self-referential state and async state machines. See
  <https://doc.rust-lang.org/std/pin/>.

## Repository-Preferred Abstractions

When the scenario matches, prefer these local building blocks before inventing new ones:

- interior mutability with synchronization: `SpinLock` and existing guard types;
- owned raw buffers: `NonNull<u8>` paired with an owner or allocation API;
- boot/firmware byte parsing: bounds checks plus `read_unaligned` by value.

Before creating a new unsafe abstraction, check whether one of these can absorb the use site.

## Review Questions Per Unsafe Site

For each nontrivial unsafe site, answer these in review:

1. Which scenario from this file does it belong to?
2. Which invariant category is actually carrying safety?
3. Why is an existing repository abstraction insufficient here?
4. Is the remaining unsafe operation the smallest possible one?
5. Would a type, guard, newtype, or wrapper eliminate caller discipline?
6. If this pattern is recurring, should the skill gain a new standard entry?

## Invalid Patterns

Avoid patterns like:

## When Reviewing

Check specifically for:

- private mutable state shared across too large a module boundary;

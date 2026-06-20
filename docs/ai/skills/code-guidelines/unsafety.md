# Unsafety

Use this file for any change that adds,
modifies, or reviews `unsafe` code.

## Mandatory Rules

- every `unsafe` block must have a preceding `// SAFETY:` comment;
- every `unsafe fn` and `unsafe trait` must include a `# Safety` rustdoc section;
- `unsafe impl` must have an explicit safety argument in nearby comments or docs;
- safety arguments must state concrete invariants and who upholds them;
- keep each `unsafe` block as small as practical,
  so the audited region is obvious;
- inside `unsafe fn`, still isolate each unsafe operation
  in an explicit `unsafe {}` block rather than relying on
  the whole function body being implicitly unsafe;
- avoid vague claims like "this is safe by construction"
  without naming the construction and the invariant.
- when a crate or module is expected to remain safe-only,
  do not introduce `unsafe` casually; push unsafe operations behind existing safe layers when possible.

## SAFETY Comment Standard

Use `// SAFETY:` comments as local proof obligations,
not as labels.

Each `// SAFETY:` comment should:

- describe the specific unsafe operation being justified;
- name the concrete invariants it relies on;
- say who established those invariants
  or what earlier check/guard/state transition established them;
- mention all relevant categories when there is more than one,
  such as validity plus alignment, or initialization plus exclusivity;
- be placed immediately above the `unsafe {}` block it justifies.

Good `// SAFETY:` comments usually answer one or more of:

- why is this pointer valid for reads or writes here?
- why is alignment satisfied, or why is unaligned access intentional?
- why is the pointee initialized as the target type?
- why is mutable access exclusive?
- why is concurrent access synchronized?
- why is this ABI/layout assumption valid?

Avoid `// SAFETY:` comments that only say:

- "pointer is valid";
- "caller guarantees safety";
- "safe because checked above";
- "FFI call";
- "this is safe by construction".

If the safety story depends on a previous check,
name that check explicitly.
If it depends on a lock, guard, state bit, ownership rule,
or current-CPU confinement, name that explicitly.

## Audit Surface

- encapsulate unsafe state in the smallest practical module;
- if private fields participate in a safety invariant,
  any code that can mutate them is part of the same audit surface;
- prefer exposing safe wrappers rather than pushing unsafe obligations outward;
- when multiple unsafe paths share one invariant,
  review them together rather than independently.

## Invariant Categories

When writing a safety argument,
spell out which kind of invariant is being relied on:

- pointer validity, lifetime, and owning allocation;
- alignment and in-bounds access;
- initialization state;
- aliasing and exclusivity of mutable access;
- thread-safety, lock ownership, or interrupt/preemption state;
- pinning, immovability, or publication ordering;
- FFI ABI, callback lifetime, and unwind assumptions.

Do not compress several independent claims into a single
"pointer is valid" statement when the code also depends on
alignment, exclusivity, or initialization.

## Writing Guidance

- explain why the operation is sound under the current invariant;
- mention the guard, lock, state check, ownership rule,
  or lifetime relationship that makes it sound;
- if the safety story depends on ordering or publication,
  say so explicitly.
- if the code relies on caller obligations from `# Safety`,
  say which obligations are being discharged at the call site.
- for `unsafe impl Send` or `unsafe impl Sync`,
  explain the real concurrency model rather than restating the trait name.

## Preferred Techniques

- prefer expressing unsafe invariants in types and API shape
  rather than relying only on comments or caller discipline;
- when a plain `T` cannot legally represent the state,
  introduce a type that does, such as `MaybeUninit<T>`,
  `NonNull<T>`, pin wrappers, newtypes for raw handles,
  or guard/token types for validated access;
- use types to carry as much of the safety story as possible,
  then use the smallest remaining `unsafe` block and `// SAFETY:`
  comment to discharge what cannot be encoded statically;
- prefer a safe wrapper API around raw-pointer or FFI internals
  when the invariant can be centralized;
- when an `unsafe fn` has cheap, deterministic precondition checks,
  add debug assertions for them instead of leaving all checking implicit;
- prefer `MaybeUninit<T>` for uninitialized storage
  rather than constructing invalid `T` values;
- when initializing fields in place,
  prefer `MaybeUninit`, `write`, and raw field pointers
  over creating references to uninitialized memory;
- prefer existing safe helpers for user memory, MMIO,
  refcounting, pinning, and synchronization
  instead of open-coding raw operations.

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
- replace open-coded raw operations with existing typed APIs;
- encode caller obligations in types;
- shrink unsafe blocks to the minimal audited operation;
- improve `SAFETY:` arguments and `# Safety` contracts.

Do not treat “fewer `unsafe` tokens” as success by itself.
Moving raw memory operations into a “safe” helper that merely hides
the same preconditions is not a governance win.

## Scenario Catalog

Use the first matching scenario.
The review should name the scenario explicitly.

### User Memory And Syscall ABI

Prefer:

- `UserPtr<T>` / `UserConstPtr<T>` for typed user pointers;
- `UserRead` / `UserWrite` for by-value syscall ABI objects;
- `core/kuaccess` and `process/osvm` helpers for checked copy-in/copy-out;
- `IoVectorBuf`, `VmBytes`, `VmBytesMut`,
  or equivalent existing wrappers for bulk or vectored buffers.

Avoid:

- direct dereference of user pointers in syscall code;
- ad hoc `from_raw_parts` over user addresses;
- copying raw bytes into arbitrary `T` without a `UserRead`-style validity contract.

Repository examples:

- [posix/types/src/ptr.rs](/home/laoyekang/code/x-kernel/posix/types/src/ptr.rs:41)
- [core/kuaccess/src/lib.rs](/home/laoyekang/code/x-kernel/core/kuaccess/src/lib.rs:30)

### Uninitialized Storage

Prefer:

- `MaybeUninit<T>` for storage that is not always initialized;
- `[MaybeUninit<T>; N]` plus an initialized-length/count invariant;
- `write`, `assume_init`, and `assume_init_drop`
  only at the point where initialization state is proven;
- raw field pointers plus `write` for field-by-field initialization.

Avoid:

- `mem::zeroed()` or `assume_init()` for arbitrary `T`;
- creating `&T` / `&mut T` before the pointee is fully initialized;
- whole-array `assume_init()` unless initialization of every element is proven.

Repository examples:

- [api/linux_sysno/src/map.rs](/home/laoyekang/code/x-kernel/api/linux_sysno/src/map.rs:10)
- [task/kpoll/src/lib.rs](/home/laoyekang/code/x-kernel/task/kpoll/src/lib.rs:73)

Relevant standard references:

- `MaybeUninit` exists specifically to model data that may be invalid
  until initialization is complete, and invalid values for references,
  `bool`, and other types are immediate UB. See
  <https://doc.rust-lang.org/std/mem/union.MaybeUninit.html>.
- The Rustonomicon explicitly states that interpreting uninitialized memory
  as a value is UB. See
  <https://doc.rust-lang.org/nomicon/uninitialized.html>.

### Shared Mutable State

Prefer:

- `SpinLock`, `RwLock`, or other lock types that centralize
  `UnsafeCell` access and expose guard-based references;
- atomics when the shared state is actually atomic
  and the ordering is part of the invariant;
- per-CPU wrappers or current-CPU confinement
  when exclusivity comes from execution context.

Avoid:

- `static mut` for ordinary shared state;
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

Prefer:

- `klazy::Once<T>` and `Lazy<T, F>` / `LazyInit<T>` for one-time global initialization;
- explicit state machines with atomic publication only when existing once/lazy types
  do not fit the lifecycle.

Avoid:

- open-coded `static mut Option<T>` initialization;
- partially synchronized “init if null” patterns;
- duplicating once-state machines outside foundational crates without reason.

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

- [drivers/net/src/net_buf.rs](/home/laoyekang/code/x-kernel/drivers/net/src/net_buf.rs:16)
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
- using `&packed.field as *const _` on unaligned packed fields;
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

Prefer:

- a typed size calculation or cursor-offset model
  before writing structured data into a raw buffer;
- `checked_add` and explicit capacity checks
  before advancing a raw write cursor;
- `write`, `write_unaligned`, or `copy_nonoverlapping`
  only after the destination byte range is proven in-bounds;
- typed wrappers over the output buffer when the format is reused.

Avoid:

- incrementing a raw pointer through a fixed buffer
  without proving remaining capacity at each write;
- relying on a later final length field as if it proved
  earlier writes were in-bounds;
- treating "bootloader allocated this page" as proof
  that any amount of serialization will fit.

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

- existing MMIO/PIO helpers and device-resource abstractions;
- typed register wrappers or central driver glue that localizes volatile access;
- volatile reads/writes only for externally observable device memory semantics.

Avoid:

- using volatile accesses as a substitute for synchronization;
- mixing ordinary references with MMIO register semantics casually;
- open-coding fixed-address register access when a subsystem helper exists.

Repository examples:

- [drivers/arm-gic/src/gicv3.rs](/home/laoyekang/code/x-kernel/drivers/arm-gic/src/gicv3.rs:121)
- [drivers/console/src/ns16550_mmio.rs](/home/laoyekang/code/x-kernel/drivers/console/src/ns16550_mmio.rs:1)

Relevant standard references:

- volatile accesses are externally observable and are appropriate for I/O memory,
  but they are not atomic synchronization primitives. See
  <https://doc.rust-lang.org/std/ptr/fn.write_volatile.html>.

### FFI And Foreign Ownership

Prefer:

- a narrow `unsafe extern` declaration layer;
- safe wrappers that translate raw buffers and ownership into Rust types;
- `repr(C)` structs and explicit ABI newtypes at the boundary;
- explicit ownership and unwind policy in the wrapper docs.

Avoid:

- scattering raw foreign calls across higher-level logic;
- assuming foreign pointers are valid “because C gave them to us”;
- exposing foreign ownership obligations to unrelated callers.

Relevant standard references:

- the Nomicon recommends hiding raw FFI details behind a safe interface
  whenever the wrapper can validate inputs and centralize obligations. See
  <https://doc.rust-lang.org/nomicon/ffi.html>.
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

- leaving boot-entry assumptions implicit
  just because the code is architecture-specific;
- mixing ordinary initialization logic into naked-entry or
  CPU-transition blocks when it can run after the unsafe handoff;
- treating `#[unsafe(naked)]`, `#[unsafe(no_mangle)]`,
  or linker-section placement as self-justifying.

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
- Linux Rust coding guidelines require explicit `// SAFETY:`
  comments before unsafe blocks even in low-level code. See
  <https://docs.kernel.org/rust/coding-guidelines.html>.

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

- user memory: `UserPtr`, `UserConstPtr`, `UserRead`, `UserWrite`, `VmBytes`, `VmBytesMut`, `IoVectorBuf`, `core/kuaccess`;
- one-time init: `klazy::Once`, `Lazy`, `LazyInit`;
- interior mutability with synchronization: `SpinLock` and existing guard types;
- uninitialized arrays and slots: `MaybeUninit` plus explicit initialized-count invariants;
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

- `mem::zeroed()` or `assume_init()` for types whose valid bit patterns
  are not proven by the type contract;
- creating `&T` or `&mut T` before the pointee is fully initialized;
- widening the unsafe region to include ordinary safe logic
  such as error handling, logging, or branching;
- documenting only that an unsafe operation is "FFI" or "raw pointer"
  without naming the actual invariants.

## When Reviewing

Check specifically for:

- missing `// SAFETY:` comments;
- `unsafe fn` without `# Safety`;
- unsafe operations inside `unsafe fn`
  that are not wrapped in a minimal `unsafe {}` block;
- invariants that are asserted but not actually enforced;
- `unsafe` sites that rely on several conditions
  but document only one of them;
- uses of uninitialized storage that should instead be modeled
  with `MaybeUninit<T>`;
- private mutable state shared across too large a module boundary;
- `unsafe impl` added without a clear thread-safety or ownership argument;
- FFI calls or callbacks whose ABI, lifetime, ownership,
  or unwind assumptions are not stated;
- opportunities to move the unsafe edge behind an existing safe abstraction
  instead of exporting new unsafe obligations.

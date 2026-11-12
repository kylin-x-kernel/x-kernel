# kcpu_slot Design

For the current state of the unsafe boundary, the target capability model, and the phased convergence plan, see
[`unsafe-convergence.md`](unsafe-convergence.md).

`kcpu_slot` is an architecture-layer crate independent of the old `percpu`. It represents per-CPU data as hidden templates in
`.cpu_slot.template`, and the linker script reserves equally spaced copies for every possible CPU. At CPU
startup it copies the template and installs the architecture base register; the hot path accesses data directly through `base + offset`,
with no hash table or lock.

## API Constraints

- `CpuSlot::with_current_irq_disabled` provides scoped shared access for
  `T: Sync`. The masking contract belongs to kiface-cpu-api, not kcpu_slot.
  It uses `kiface_cpu_api::CpuLocalIrqIf` before selecting the CPU slot and restores
  the original IRQ mask through a private RAII guard. It neither changes task
  preemption accounting nor invokes scheduler checks. The callback must not
  block, schedule, enable IRQs, or change the CPU base. Its slot borrow cannot
  escape. This is not a lock and does not exclude NMI or remote atomic access.
  Use it for bounded shared observations or atomic updates. Keep an existing
  pin when exclusion spans queue selection and submission, multiple mutable
  slots, initialization, or a later context switch. Do not use it inside the
  IRQ-mask provider itself: that would recurse through `kiface_cpu_api::CpuLocalIrqIf`.
- `cpu_slot!` is for statically bit-copyable templates; complex initialization should be constructed separately for each CPU by the upper layer.
- `cpu_slot_cell!` provides `UnsafeCell<T>` and may only be used when the caller has established an exclusive or local-CPU
  access protocol; `CpuSlotCell::read`/`write` offer a convenient entry point for single-word reads and writes, while read-modify-write
  and atomic semantics are still supplied by the concrete architecture implementation; a plain read-modify-write must not be mistaken for an atomic operation.
- `CpuPin` is an execution-context contract, implemented by upper-layer guards: `kspin::IrqSave`,
  `kspin::NoPreemptIrqSave`, and `kspin::NoPreempt` all implement it (with `preempt` disabled and only
  cooperative scheduling, `NoPreempt` likewise pins the current CPU). It only expresses "the CPU is pinned" and does not disable
  preemption by itself; the scheduler/interrupt layer is responsible for creating the guard. `kspin` guards obtain the current logical CPU through `kspin::CurrentCpuIf`
  (provided by `kplat`); these are upper-layer contracts between `kspin` and `kplat`,
  and `kcpu_slot` only defines the `CpuPin`/`CpuLocalGuard` traits and does not itself depend on `kspin`/`kplat`.
- `CpuLocalGuard` capability trait: requires no migration and no local IRQ reentrancy; it is implemented by `kspin::NoPreemptIrqSave` and by `BootCpuToken` during exclusive CPU bring-up. The safe accessors `CpuSlot::get`, `CpuSlotCell::get`,
  `read`/`write`, `current_shared`/`shared_current`, etc. are open only to it or to `T: Sync`;
  general mutable references are kept only as the explicitly unsafe `get_mut_unchecked`. `CpuLocalGuard` only proves
  mutual exclusion of the execution context; it cannot prove unique borrowing of an object across different nested guards.
- The low-level `*_unchecked` entry points keep `unsafe` and the wider `CpuPin` bound, and are reserved only for
  initialization and special architecture paths.
- `CpuSlot::at`, `CpuSlotCell::shared_at`, `CpuSlot::current_shared`, and
  `DynamicCpuSlot::get_remote` open up shared reads for `T: Sync`; no unconditional API is provided for
  remote writes.
- `begin_cpu_init` copies the template and sets the register; `BootCpuToken::publish` publishes a checked `CpuArea` that can be accessed remotely after CPU-local
  initialization finishes.
- `CpuSlotChunk` uses backing memory provided by the platform; `DynamicCpuSlot` is initialized separately on each CPU, and
  on initialization failure the already constructed objects are rolled back, and when the handle is dropped they are destructed CPU by CPU. A chunk uses shared
  bump metadata, allowing multiple dynamic handles to be alive at the same time. When the backing or stride alignment is not satisfied, `SlotInitError::Misaligned` is returned, distinguishing it from space exhaustion, `NoSpace`. In addition to type alignment, the backing base address
  and the stride must also be multiples of `CPU_SLOT_AREA_ALIGN`; otherwise objects of different CPUs would share a cache
  line.
- A `DynamicCpuSlot` handle can be shared across threads (`Sync`) under `T: Send`: after construction the handle's fields
  are immutable, and each thread observes only the instance of its own CPU through a guard; `get_remote`, the only way to share a single value across threads,
  separately requires `T: Sync`. The chunk itself stays non-`Sync` because of its bump cursor,
  and after allocation finishes the handle lifetime is fixed through a leaked `&'static CpuSlotChunk`.

## Link Layout

The root linker script defines the template LMA, the template size, and `.cpu_slot.template`. The template resides at the image load address,
and the template VMA is zero; the public descriptor resides in the normal data region. The template image and the CPU areas do not overlap.

The area stride is the actual template size rounded up to `CPU_SLOT_AREA_ALIGN`, and the area start address is also aligned to the same
value; this constant is provided by `kaddr_layout` and substituted into the linker script via `%CPU_SLOT_AREA_ALIGN%`, so the Rust-side
`stride()` and the linker cannot drift apart. It takes the upper bound of the largest data cache line among supported platforms (currently
128B), so **adjacent CPU areas never share a cache line**: a lock word in one CPU area will not
falsely share a line with data of another CPU. This invariant still holds when the template grows, because the alignment is based on a compile-time constant
rather than on the template layout.

This namespace is completely separate from the old `.percpu`; the migration is complete, and the target image keeps only
`.cpu_slot.template`, no longer contains the old `.percpu` region, and no longer depends on the crates.io
`percpu` crate and its `non-zero-vma` feature.

Unsupported architectures (anything other than x86_64/aarch64/riscv/loongarch64) are rejected through
`compile_error!` in non-test builds, to prevent base register accesses from silently failing.

RISC-V uses `gp` as the per-CPU base register; `tp` is already used by kernel TLS and cannot also serve as the
per-CPU base.

## Future Extensions

Dynamic slots implement per-CPU initialization and failure rollback through `CpuSlotChunk::alloc`; remote reads use
`DynamicCpuSlot::get_remote`, which is open only for `T: Sync` and provides no remote write API.

The first phase of the safe access API is complete: the real `kspin` guards implement `CpuPin`,
`NoPreemptIrqSave` implements `CpuLocalGuard`, and ordinary shared and by-value slot accesses now have safe
methods constrained by guards; general mutable references are kept only behind the `*_unchecked` unsafe boundary.

The migration of platform identity and scheduler state is complete: `current_task_ptr`, the BSP flag, `KCPU_ID`
(the current CPU identity, and the source of every guard's `current_cpu()`), and call sites such as ktask/kirq/kalloc
now all sit on register-addressed static slots or on dynamic slots managed by `kplat`; the old `percpu`
dependency, the `.percpu` region, and the `non-zero-vma` feature have all been removed.

Remaining extensions:

- Fill in the IRQ/trap context and scheduler pinned context implementations according to the `CpuLocalGuard` contract.
- The atomic `load/store/add/bit*` fast paths of `CpuSlotCell` are to be supplied by the concrete architecture modules.

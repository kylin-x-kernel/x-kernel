# kcpu_slot Security Notes

For the complete inventory of unsafe types, the classification of external raw accesses, and the convergence plan, see
[`unsafe-convergence.md`](unsafe-convergence.md). This document retains the core safety
invariants that the current implementation must satisfy.

The unsafe boundary of `kcpu_slot` is concentrated in three places: architecture register reads and writes, template copying, and slot pointer
arithmetic.

`CpuSlot::with_current_irq_disabled` retains `T: Sync`: masking ordinary IRQs
does not exclude NMI or remote access. It returns no slot borrow and restores
the caller's IRQ mask on return or unwind. Callbacks must not block, schedule,
enable ordinary IRQs, or change the CPU base. It adds no mutable-reference or
pointed-object lifetime guarantee; those remain the payload owner's duty.

The caller must guarantee that:

1. The template LMA comes from the same image as the linker script, and the target area of `begin_cpu_init` is unique to that CPU,
   writable, and covers `template_size()` bytes;
2. After the base register is installed, the CPU must not migrate during access. Safe accessors require a `CpuLocalGuard` (currently `kspin::NoPreemptIrqSave`, plus `BootCpuToken` during exclusive CPU bring-up of an unpublished area), which excludes both migration and local IRQ reentrancy;
   the `*_unchecked` entry points require only `CpuPin`, and excluding IRQ reentrancy is established by the caller;
3. Aliasing rules: safe shared and by-value access must not overlap unsafe mutable access. Generic mutable references are exposed through `*_mut_unchecked`, where the caller must prove the object is uniquely borrowed, and through the public unsafe `CpuSlotCell::current_shared_mut`, which returns `&'static mut T` without a pin and requires the caller to be the single execution context on this CPU and not alias the value. Cross-CPU access must not be disguised as local exclusive access; `CpuSlot::at` and `DynamicCpuSlot::get_remote` are available only for `T: Sync`, and unlocked remote writes are not provided by the base API;
4. The section symbols in the linker script and the crate's build artifacts come from the same kernel image, and must not be forged by untrusted
   modules;
5. The backing memory of `CpuSlotChunk::from_raw_parts` remains
   valid until all dynamic handles are dropped; a `DynamicCpuSlot` must not outlive the lifetime of its chunk;
6. The static macros `cpu_slot!`/`cpu_slot_cell!` accept only types implementing the public `unsafe trait StaticSlotValue`, while `cpu_slot_cell_with_template!` certifies the exact initializer at the call site. Types that need construction or destruction must not be placed in the template section and must use the per-CPU initializer of a dynamic slot.

The architecture base register is privileged kernel state. It may be installed only by the platform's architecture initialization path and cannot be arbitrarily rewritten through
ordinary safe APIs; incorrect settings would cause arbitrary slot accesses to go out of bounds. x86 uses GS-relative
access and GS base initialization, and does not assume that FSGSBASE is enabled. Unsupported architectures cannot be
compiled in non-test builds, so that base register accesses do not silently fail.

The platform is responsible for publishing template readiness and allocating the backing area for each CPU, and a CPU is allowed to enter slot-dependent code only after calling `begin_cpu_init`.
`kcpu_slot` does not duplicate the BSP/AP state machine, and is not responsible for the
IRQ, preemption, or CPU online lifecycle.

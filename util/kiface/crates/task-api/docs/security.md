# kiface-task-api security

This crate is a trusted kernel contract boundary. It contains no runtime state,
allocation, manual unsafe code, task pointers, or user-input handling. `kiface`
generates static Rust-ABI calls; the linked provider must obey the documented
context, identity, lifetime and progress obligations.

Blocking-context queries require initialized CPU context outside NMI, preserve
exclusion and pending work, and return false without a usable current task.
They are snapshots, not permits to block after changing execution context.

Preemption credits must balance on the same task and must not cross initial
current-task installation. The provider must not schedule on acquisition or
unchecked release. Checked release may schedule only when surrounding context
permits it; consumers combining IRQ exclusion must restore masks first. These
operations neither mask IRQs nor provide NMI mutual exclusion.

NonZeroU64 excludes the unlocked sentinel, but key uniqueness and stability
remain provider obligations. Keys must survive task migration and must not be
reused while a consumer still records the owner. Absent current-task state
must not fabricate an identity. Queries must not block or unwind on valid use.

Yield and wait require sleepable task context. The pinned borrowed future must
not escape the synchronous wait or move to another task; completion requires
correct wake registration and scheduler progress. This interface offers no
cancellation, timeout, or recovery from invalid execution context.

Diagnostic addresses are opaque identities, never dereferenced. Hooks may run
under IRQ or preemption exclusion and must not block, unwind or reenter the
calling lock. They must not determine lock admission. Numeric keys and lock
addresses remain kernel diagnostics, not public process identifiers.

# krlimit design

## Purpose and background

`krlimit` defines resource-limit values for process creation and limit syscalls.
It separates the Linux-indexed data model from `kresources` locking/update policy
and from actual resource enforcement in consuming subsystems. It has no process
lookup, authorization, allocation accounting, or current-task dependency.

## Scope and architecture

`src/lib.rs` contains `Rlimit`, `Rlimits`, `FILE_LIMIT`, `SYSCTL_NR_OPEN_DEFAULT`, conversions, indexing,
and kernel unit tests. `linux_raw_sys::general` supplies resource indices.
`Rlimit` holds public `current` (soft) and `max` (hard) `u64` values; `Rlimits` owns
a fixed `[Rlimit; RLIM_NLIMITS]`. Consumers index it with Linux `u32` resource IDs.
`Index` and `IndexMut` return references directly and use array bounds checks.

## Initialization algorithm and decisions

`Rlimits::new(user_stack_size)` begins with unlimited pairs (`u64::MAX`) and then
sets stack soft/hard to the supplied size in bytes, file count to 1024, core dump
soft to zero, locked memory to 8 MiB, and message queue bytes to 819200.
Signal-pending count, nice, and realtime-priority limits are zero pairs.
`RLIMIT_NPROC` stays unlimited; a zero value would break consumers that use
`sysconf(_SC_CHILD_MAX)` to size their process bookkeeping.

The fixed stack and descriptor limits match current kernel capacities rather
than promising growable Linux defaults. Other entries retain unlimited pairs.
`Rlimit::new` and `From<u64>` construct values without validation: the latter
uses the same value for soft and hard. Callers applying a policy must validate
before storing. There is no explicit state machine.

## Execution context and concurrency

These are pure value operations, with no locks, heap allocation, blocking,
CPU-local data, mappings, or scheduler requirement. They can be used in early
boot and interrupt context if callers already own valid access to the value.
Shared mutation must be serialized externally; `kresources` uses an `RwLock`.

## Resource lifecycle and limitations

Construction initializes every table slot; values have no custom drop or owned
external resources. Storage is released with the containing object. Indexing an
invalid resource ID panics. The data model does not enforce soft <= hard,
privileges, or resource consumption, and unlimited is a sentinel rather than a
measurable quota.

# linux_sysno — Design

## Purpose and scope

`linux_sysno` provides Linux syscall identifiers and errno carriers for adapters
such as `ksyscall`, and fixed-storage collections for consumers that classify
syscalls. It does not execute a syscall, validate user pointers, enforce access
control, or guarantee that a listed call is implemented by X-Kernel.

The complete source scope is `src/lib.rs`, `src/args.rs`, `src/macros.rs`,
`src/map.rs`, `src/set.rs`, `src/errno/{mod,macros,generated}.rs`, and
`src/arch/{mod,macros,aarch64,arm,loongarch64,riscv64,x86_64}.rs`.
`build.rs` detects Thumb-compatible target names. Tests live under `ut-tests/`
and are mounted with `unittest::path_mod_test!`. Architecture tables and errno
constants are checked-in ABI data; no network fetch or table regeneration occurs
during a normal build.

## Architecture and interfaces

```text
raw register number ──> Sysno::new ──> target-specific Sysno
raw argument values ──> SyscallArgs / syscall_args!
raw return register ──> Errno::from_ret ──> Result<usize, Errno>
validated Sysno ──> SysnoSet (bitset) / SysnoMap<T> (membership + slots)
```

`lib.rs` re-exports the current architecture's `Sysno`, enabled architecture
modules, argument carrier, errno types, set/map and iterators. The architecture
macro generates a non-exhaustive enum, numeric/name lookup and iteration.
The table's first identifier and its largest identifier define the numeric span;
`last()` returns the largest identifier regardless of declaration order. Gaps
occupy storage but are not valid keys. Numeric `From<i32/u32>` conversions panic on gaps; the
fallible `new` constructor is the syscall-boundary entry point. Parsing names
is exact and case-sensitive.

`args.rs` packs six `usize` values; shorter array conversions and `syscall_args!`
zero the missing registers. Packing is independent of the number's ABI: callers
must supply the right arity and interpret signed values, addresses and lengths.
`errno` exposes named positive constants plus an unchecked signed carrier.
`ErrnoSentinel` supplies libc-style all-ones sentinels, without thread-local errno.
`kerrno` re-exports `Errno` as `LinuxError` and adds a separate kernel-kind domain.

## Configuration

Native architecture selection uses `target_arch`. Features `aarch64`, `arm`,
`loongarch64`, `riscv64`, and `x86_64` additionally expose foreign tables; `all`
enables those table modules, not every Cargo feature. The unqualified set and
map still use the native `Sysno`. `tee` and `tipc` enable entries where the
checked-in table declares those extensions, so they can enlarge the numeric
span. They do not install dispatch handlers. `serde` enables carrier serialization
and enum representation support; `with-serde` is the deprecated compatibility
feature. `thumb-mode` is a retained feature selected by `build.rs` on matching
targets; it does not supply a trap backend.

## Algorithms and state

`SysnoSet` stores one bit per numeric offset from the first identifier. Key
lookup, insertion and removal access one word. Union/intersection/difference,
counting and emptiness checks traverse words; iteration scans set bits in numeric
order. Unused gaps are kept clear by the typed construction paths. There is no
runtime registration or global mutable set.

`SysnoMap<T>` combines that membership set with an inline `MaybeUninit<T>` array.
An absent bit means the slot must not be read or dropped as `T`. Insertion into
an absent slot initializes it; replacement returns the former value; removal
clears membership and transfers ownership out. Borrowed iterators expose only
occupied slots. `from_slice` requires `Copy` and lets the last duplicate win;
`init_all` clones one value per valid identifier. Indexing requires an occupied
key and panics otherwise; `get` returns `None` instead.

`Errno::from_ret` recognizes register encodings of -4095 through -1 and returns
positive errno; all other words, including the encoding of -4096, remain success.
`Errno::new` stores any `i32`, and `is_valid` tests only `< 4096`, including zero
and negatives. Consumers needing named or positive errors must check separately.

## Execution context and concurrency

Pure lookup, packing and bitset operations do not sleep, allocate, or require a
current thread, CPU-local state, mappings or platform initialization. They can
be used in early boot or interrupt context when storage is available. Map
operations inherit `T`'s clone/drop/formatting behavior: those callbacks can
allocate, block or panic, so IRQ suitability is not unconditional. Large inline
maps can exceed a kernel stack budget, especially with extension-number gaps.

There are no production locks or atomics. Mutation requires `&mut self`;
shared mutation requires external synchronization. Borrowed iteration prevents
simultaneous safe mutation. Compiler auto traits determine transfer/sharing from
`T`; formatting additionally depends on the caller's formatter.

## Ownership, drop and decisions

Collections use inline arrays instead of hashing or heap nodes to give predictable
key lookup and const construction. Numeric gaps consume space; this is the cost
of direct indexing. Sets and argument/errno carriers own no external resources.
Maps own their occupied values and drop them on `clear`/`Drop`; replaced/removed
values belong to the caller. Value destructors must not unwind: `clear` clears
the membership set after the drop loop, so a panicking destructor leaves stale
membership. This is a documented limitation, not unwind-safe cleanup.

`syscall!` and `raw_syscall!` remain exported legacy macros, but their expansion
requires absent `syscall0..6` or `raw::syscall0..6` symbols. They cannot be used to
invoke calls in this fork. Their compile-fail examples record this limitation;
adding an invocation backend is outside this documentation change.

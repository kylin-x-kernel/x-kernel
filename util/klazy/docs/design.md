# klazy — Design Document

## Purpose

`klazy` provides `no_std`-compatible one-time initialization and lazy evaluation synchronization primitives. These primitives are
the building blocks for static lazy initialization of kernel services in x-kernel (such as tracing, memory management, TEE
services, and cryptography modules).

## Background

`std::sync::Once` and `std::sync::LazyLock` depend on the standard library. x-kernel runs in a
bare-metal `no_std` environment and cannot use these types. This module provides equivalent functionality without depending on `std`,
OS threads, or condition variables.

The reference implementation is adapted from `spin::once` and `std::sync::SyncLazy`.

## Scope

Source files involved:

```
util/klazy/
├── src/
│   ├── lib.rs
│   ├── once.rs
│   ├── lazy.rs
│   └── lazy_static.rs
└── Cargo.toml
```

## Architecture

Two types, built in layers:

```
Lazy<T, F>  ──uses──>  Once<T>
```

| Component | Responsibility |
|------|------|
| `Once<T>` | A thread-safe cell that guarantees a closure runs only once. A four-state atomic state machine that holds the initialized value |
| `Lazy<T, F>` | Wraps a factory function `F` on top of `Once<T>` and triggers initialization on first access through `Deref`. Supports `const` construction, which makes it suitable for use as a static variable |

## Calling Constraints / Execution Context

`klazy` is designed for initialize-once scenarios during early kernel boot and normal runtime,
but callers must satisfy the following constraints:

- **Callable during early boot**: the module does not depend on the scheduler, condition variables, or a process/thread context.
- **Callable with interrupts disabled or on non-sleepable paths**: internally it only uses atomics and spin waiting,
  and never blocks or sleeps on its own.
- **Not suitable for long-running initialization closures**: if the initialization closure runs slowly,
  concurrent callers keep spinning in the `Initializing` state, wasting CPU.
- **No current process/thread required**: the API only depends on the current execution path,
  not on scheduler state such as the "current task".
- **Limited reentrancy**: the initialization flow of a given `Once` / `Lazy` instance must not recursively enter itself,
  otherwise the current execution path may spin waiting for its own initialization to complete.
- **Depends on basic atomic availability**: correctness depends on the target architecture providing a usable `AtomicU8`
  and memory ordering semantics; the currently supported architectures satisfy this prerequisite.

## State Machine (Once)

```
                  call_once()
  Uninitialized ────────────> Initializing ────> Ready
       │                           │
       │                      panic!
       │                           │
       │                           v
       └─────────────────────  Failed
                                  │
     try_call_once()              │
     returns Err, reset to        │
     Uninitialized                │
                                  v
                             poison: all
                             later calls panic
```

The state is stored as a `u8` discriminant in an `AtomicU8`:

| State           | Value   | Meaning                     |
|---------------|------|--------------------------|
| Uninitialized | 0x00 | Initialization has not started            |
| Initializing  | 0x01 | A thread is running the initialization closure   |
| Ready         | 0x02 | The value is initialized and can be read         |
| Failed        | 0x03 | The initialization closure panicked (poisoned) |

### State Transitions

| From            | To            | Trigger                        |
|---------------|---------------|--------------------------------|
| Uninitialized | Initializing  | The first thread wins the CAS               |
| Initializing  | Ready         | The initialization closure returns Ok              |
| Initializing  | Failed        | The initialization closure panics (PanicGuard) |
| Initializing  | Uninitialized | `try_call_once` returns Err       |
| Failed        | —             | All later calls panic (poisoned)      |

## Memory Ordering

| Operation                          | Ordering     | Reason                                  |
|-------------------------------|-----------|---------------------------------------|
| Fast-path `get()`              | `Acquire` | Pairs with the writer-side `Release`          |
| CAS contention                      | `Acquire` | Both the success and failure paths need to establish happens-before |
| store `Ready`                 | `Release` | Publishes the initialized value to all readers             |
| store `Uninitialized` (on error) | `Release` | Publishes the reset state so retrying threads see a clean state     |
| PanicGuard drop               | `SeqCst`  | Conservative choice — panic unwind is a boundary case      |

`is_completed()` uses `Acquire`, which is sufficient to pair with the `Release` store and ensures that subsequent
`get_unchecked()` calls are safe.

## Concurrency Model

- **Spin waiting**: `wait()` and `poll()` spin while the state is `Initializing`
  (`core::hint::spin_loop`) and do not use blocking primitives.

- **Fast path**: `get()` is only one `Acquire` load plus a branch — zero
  overhead once initialization has completed.

- **Slow path** (`try_call_once_slow`): a CAS loop competes for the right to initialize and correctly coordinates
  concurrent callers.

### Trade-off: Spinning vs Blocking

Spin waiting suits early kernel boot and scenarios where blocking primitives are unavailable. For initialization
closures with long execution times, spinning threads waste CPU. This is acceptable because:

1. Initialization closures in x-kernel are usually fast (register configuration, static allocation).
2. When these primitives are first used, the scheduler and condition variables are not yet available.

## Panic and Poisoning

`PanicGuard` marks the cell as `Failed` on unwind. After poisoning:

- `call_once()` / `try_call_once()` / `wait()` / `poll()` all panic.
- `get()` returns `None` (it does not panic — this allows reading the pre-poisoning state, although a fresh cell has no such state).

This is consistent with the poisoning semantics of `std::sync`: a panic means the state may be corrupted, and all later accesses
are rejected.

For `Lazy<T, F>`, the factory function is consumed through `Cell::take()` on the first `force()`.
If the initialization closure panics, the factory function is lost and the `Lazy` becomes permanently unusable (consistent with
the behavior of `std::sync::SyncLazy`).

## Design Decisions

### Why Not `spin::Once`

The upstream `spin` crate provides similar functionality. We bring the design into `klazy` in order to:

- Control the API scope (adding `try_call_once`, `initialized()`, etc.).
- Be unaffected by the `spin` release cadence, ensuring stability.
- Keep the dependency tree of kernel crates minimal.

### Why Not `atomic_polyfill` / portable-atomics

We use `core::sync::atomic` directly. A polyfill is needed on architectures where `AtomicU8` is not lock-free. On the currently supported targets (x86_64, aarch64, riscv64, loongarch64),
`AtomicU8` is lock-free on all of them, so no extra dependency is needed.

### Deref Semantics of the `lazy_static!` Macro

For each `static ref NAME: T`, `lazy_static!` generates an **independent, zero-sized
wrapper type** (using the `paste` crate for ident concatenation, with internal symbols named like `NAME__LazyRef`)
that implements `Deref<Target = T>`. This matches the dispatch
semantics and isolation model of the upstream `lazy_static` crate:

- **Dispatch semantics**: calling a trait method on the static (such as `NAME.clone()` or
  `NAME.lock()`) goes through autoderef to the inner value `T`, rather than cloning or operating on the outer
  wrapper.
- **Type isolation**: each static has its own wrapper type, so two statics are not interchangeable even if their inner value types
  are the same — the compiler rejects passing `STATIC_A` as `STATIC_B`.

The wrapper type deliberately does **not** implement `Clone`/`Copy`. Otherwise `NAME.clone()` would match
the wrapper's own `Clone` first and never reach `T::clone`. Leaving these two traits unimplemented is exactly the key to
`NAME.clone()` resolving to `T::clone` (for example, cloning an inner `Arc`).

The wrapper type's visibility follows the user-declared `$vis` (`pub`/`pub(crate)`/private) and is marked
`#[doc(hidden)]`, so that a `pub static` can be referenced across crates, while the wrapper type itself does not
appear in the rendered API documentation. `paste` is used only for compile-time ident concatenation; it is a proc-macro and
does not enter the runtime binary.

Historically, `lazy_static!` generated `NAME` directly as a `&'static Lazy<T>` reference.
In that form, `NAME.clone()` went to the `Clone` of `&T` (cloning the reference itself),
autoderef did not occur, and so callers had to write `(**NAME).clone()` by hand to get a clone of the inner
`Arc`. The current independent wrapper eliminates this workaround — code
migrating from the old form should change `(**NAME).clone()` back to `NAME.clone()`.

## Drop Behavior

`Once<T>` implements `Drop`: when the state is `Ready`, the inner `T` is dropped
through `ptr::drop_in_place`. Dropping an uninitialized cell does not touch the `MaybeUninit` storage
(no UB).

`try_into_inner` / `into_inner_unchecked` transfer ownership of the value to the caller; before
extraction, `ManuallyDrop` is used to suppress the destructor of the consumed cell, preventing a cell that is still `Ready`
from dropping the already moved-out value a second time.

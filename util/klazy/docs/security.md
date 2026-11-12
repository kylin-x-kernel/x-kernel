# klazy — Security and Reliability Analysis

## Overview

`klazy` is a low-level synchronization primitive widely used in x-kernel, responsible for the one-time initialization of kernel services.
It contains `unsafe` code involving interior mutability and atomic state transitions. Incorrect use or a broken
invariant will lead to undefined behavior.

## Trust Model

```
Caller (kernel subsystem)
   │
   │ safe API: call_once, get, wait, poll, is_completed
   │
   v
┌──────────────────────────────────┐
│  Once<T> / Lazy<T, F>           │
│                                  │
│  ┌── unsafe boundary ──────────┐ │
│  │ extract_value_unchecked     │ │
│  │ get_value_unchecked         │ │
│  │ as_mut_ptr                  │ │
│  │ MaybeUninit write/read      │ │
│  └─────────────────────────────┘ │
└──────────────────────────────────┘
```

- **Callers of the safe API** trust `klazy` to maintain its invariants correctly.
- **Callers of the unsafe API** (`get_unchecked`, `get_mut_unchecked`,
  `into_inner_unchecked`, `as_mut_ptr`) must themselves prove that initialization has completed.

## External Boundary / Attack Surface

`klazy` itself is not an I/O, device, or user-space boundary module;
its main attack surface comes from the initialization closures passed in by callers and from misuse of the unsafe API.

Upon inspection, this module:

- **does not directly access user memory**;
- **does not directly operate on MMIO / PIO registers**;
- **does not directly manage DMA buffers or device-owned memory**;
- **does not depend on FFI, inline assembly, or architecture-specific raw interfaces**;
- **does not directly parse bootloader, firmware, device tree, or ACPI input**;
- **does not directly handle external input from filesystems, networks, or IPC**.

Therefore the threat analysis of this module focuses on:

- whether callers satisfy the initialization ordering and the unsafe preconditions;
- availability problems caused by an initialization closure that panics or runs for a long time;
- CPU consumption of concurrent callers while spin-waiting;
- whether callers bypass state checks through the unsafe API.

## unsafe Code Inventory

### 1. `Status::from_raw` (`util/klazy/src/once.rs:101`)

```rust
fn from_raw(raw: u8) -> Self {
    match raw {
        0x00 => Self::Uninitialized,
        0x01 => Self::Initializing,
        0x02 => Self::Ready,
        0x03 => Self::Failed,
        _ => unreachable!("invalid Once status: {raw:#x}"),
    }
}
```

**Invariant**: the `u8` value must be a valid `Status` discriminant (0x00–0x03).

**Why it is safe**: state decoding now goes through an explicit match and no longer relies on `transmute`. An invalid value
immediately triggers `unreachable!`, instead of reinterpreting an unvalidated discriminant as an enum.

### 2. `Once::get_value_unchecked` (`util/klazy/src/once.rs:419`)

```rust
unsafe fn get_value_unchecked(&self) -> &T {
    &*(*self.storage.get()).as_ptr()
}
```

**Invariant**: the state must be `Ready` (the value is initialized).

**Callers**:
- `get()` — protected by an `Acquire` load that checks for `Status::Ready`.
- `poll()` — protected by matching on `Status::Ready`.
- `try_call_once_slow` — called after the `Release` store of `Ready`.
- `get_unchecked()` — the caller is responsible (there is a `debug_assert` in debug builds).

### 3. `Once::try_call_once_slow` value write (`util/klazy/src/once.rs:306`)

```rust
let storage_ptr = (*self.storage.get()).as_mut_ptr();
storage_ptr.write(initialized_value);
```

**Invariant**: the caller holds exclusive write permission (the CAS to `Initializing` succeeded),
and at this point no concurrent reader can observe the `Ready` state.

### 4. `Once::as_mut_ptr` (`util/klazy/src/once.rs:413`)

```rust
pub fn as_mut_ptr(&self) -> *mut T
```

**Invariant**: reading through this pointer before initialization is UB. Writing through it may be used for FFI interoperation,
and the caller bears all responsibility.

### 5. `Once::get_mut_unchecked` (`util/klazy/src/once.rs:496`)

```rust
pub unsafe fn get_mut_unchecked(&mut self) -> &mut T
```

**Invariant**: the value must already be fully initialized before the call.

**Why it is safe**: this API requires exclusive `&mut self`,
so it cannot access the same storage concurrently with other readers / writers.
However, an exclusive borrow by itself does not mean the value has been initialized;
proving this remains the caller's responsibility.

### 6. `Once::into_inner_unchecked` (`util/klazy/src/once.rs:527`)

```rust
pub unsafe fn into_inner_unchecked(self) -> T
```

**Invariant**: when `self` is consumed, the inner value must already be fully initialized.

**Why it is safe**: once `self` has been consumed by value, there is no other concurrent access path;
a valid `T` must already have been written into the `MaybeUninit<T>`. The extraction function first wraps the cell in `ManuallyDrop`,
then moves the value out with `read`, which prevents the cell, still in the `Ready` state, from dropping the value again.

### 7. `unsafe impl Sync/Send for Once<T>` (`util/klazy/src/once.rs:76`)

```rust
unsafe impl<T: Send + Sync> Sync for Once<T> {}
unsafe impl<T: Send> Send for Once<T> {}
```

**Invariants**:

- With shared access to `Once<T>`, the write to uninitialized storage must be correctly synchronized with the read of the initialized value.
- Only the execution path that wins the CAS may write to the inner `MaybeUninit<T>`.
- All shared reads derived from `&Once<T>` must happen after the `Acquire`/`Release`
  pairing on `Status::Ready`.

**Why it is safe**: the concurrency semantics of `Once<T>` are protected by the internal atomic state machine and memory-ordering constraints.
`T: Send + Sync` guarantees that the initialized value can be shared across threads;
`T: Send` guarantees that `Once<T>` can be moved between threads without breaking the ownership semantics of the inner value.

### 8. `unsafe impl Sync for Lazy<T, F>` (`util/klazy/src/lazy.rs:91`)

```rust
unsafe impl<T, F: Send> Sync for Lazy<T, F> where Once<T>: Sync {}
```

**Invariants**:

- Although `factory: Cell<Option<F>>` is not `Sync`, it may be consumed only once, on the one-time initialization path of `force()`.
- A concurrently accessible `&F` must not be derived from `&Lazy<T, F>`.
- The shared-access safety of `Lazy` depends entirely on the synchronization of the underlying `Once<T>` and on `Cell::take()` happening only once.

**Why it is safe**: `force()` uses `Once::call_once()` to guarantee that only one execution path can obtain and consume
`factory`; all other paths only read `&T` after initialization has completed. Therefore `F` only needs to be `Send`
and does not need to be `Sync`.

### 9. `Once::Drop` (`util/klazy/src/once.rs:559`)

```rust
self.storage.get_mut().assume_init_drop();
```

**Invariant**: called only when the state is `Ready`. Exclusive `&mut self` guarantees there is no concurrent access.

## Memory Safety Invariants

The following invariants must hold at all times:

1. **Single writer**: only the thread that succeeds in the CAS (from `Uninitialized` to `Initializing`)
   may write `storage`.

2. **No read before Ready**: before the state is stored as `Ready` with `Release` memory ordering,
   `storage` must not be read as a `T`.

3. **AtomicStatus validity**: the underlying `AtomicU8` must always hold a valid `Status`
   discriminant. This is guaranteed by making `AtomicStatus` the only interface and accepting only `Status` values (rather than
   raw `u8`).

4. **No double-drop**: `Drop` calls `drop_in_place` only when the state is `Ready`.
   The `try_call_once` error path does not drop a value that was never written; the consuming extraction path uses
   `ManuallyDrop` to suppress the destruction of the old cell, handing the sole destruction responsibility to the owner of the returned value.

## Thread Safety

| Type           | `Send` condition            | `Sync` condition          |
|---------------|------------------------|----------------------|
| `Once<T>`     | `T: Send`              | `T: Send + Sync`    |
| `Lazy<T, F>`  | Automatic (all fields are `Send`) | `Once<T>: Sync`     |

`Lazy` does not require `F: Sync`, because the factory function is consumed via
`Cell::take()` under the protection of the `Once` synchronization mechanism — only one thread will read `F`.

## Threat Analysis

| ID | Threat Description | Impact Level | Trigger Condition | Mitigation |
|------|----------|----------|----------|----------|
| T-01 | Calling `get_unchecked()` on an uninitialized `Once` returns a reference to uninitialized memory | High | The caller calls `get_unchecked()` directly without first confirming `is_completed()` | Detected by `debug_assert` in debug builds; release builds rely on correct use by the caller |
| T-02 | Reading through a pointer obtained from `as_mut_ptr()` before initialization | High | The caller does not follow the documented constraint of `as_mut_ptr()` | The method docs state that accessing the pointer before initialization is UB; callers bear the burden of proof; no runtime check |
| T-03 | A panic in the initialization closure causes `PanicGuard` to mark the state as `Failed`, and all subsequent calls panic permanently | Medium | The closure contains code that may panic (such as out-of-bounds access or division by zero) | `try_call_once` provides a non-panicking alternative path; on error it resets to `Uninitialized` so a retry is possible |
| T-04 | Spin-waiting has no timeout, so spinning threads waste CPU when the initializing thread is preempted | Low | The initialization closure runs for too long, or scheduler latency is high | Intended only for fast-initialization scenarios; documentation constraints discourage use on hot paths |
| T-05 | `AtomicU8` is not lock-free on the target architecture, and a global lock may be used internally | Low | Running on an architecture that does not support native `AtomicU8` | All currently supported targets (x86_64/aarch64/riscv64/loongarch64) are lock-free |
| T-06 | After the `Lazy` factory function has been consumed by `Cell::take()`, a second `force()` panics | Medium | `force()` is called again after its first call failed due to a panic | Protected by the `Once` poisoning mechanism; the second call panics directly (consistent with `std::sync` semantics) |

## Failure Mode and Effects Analysis (FMEA)

| ID | Failure Mode | Cause | Local Effect | System Effect | Severity | Mitigation |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | `get()` returns `None` for an uninitialized cell and the caller does not handle it | Incorrect call ordering; initialization has not completed yet | Functionality that depends on the value is skipped | The subsystem that uses the value loses functionality | 3 | Returning `Option` forces the caller to handle it; using `wait()` or `call_once()` is recommended |
| F-02 | The initialization closure panics and the cell is permanently poisoned | A bug in the closure (out-of-bounds access, assertion failure, etc.) | That `Once` is never usable | Kernel services that depend on that `Once` cannot start | 2 | `PanicGuard` marks the state `Failed` to prevent UB; `try_call_once` provides a retryable path |
| F-03 | Multiple threads race on `call_once`, and threads that lose the CAS spin-wait | Normal concurrent scenario | CPU usage by the spinning threads | Brief CPU waste, no functional impact | 4 | `spin_loop` hints the CPU to pause; returns immediately once initialization completes |
| F-04 | `Once` is leaked via `mem::forget` and the inner `T` is not dropped | Misuse by external code | Resources held by `T` are not released | Memory or resource leak | 3 | Known issue in the Rust ecosystem; not a memory-safety problem |
| F-05 | After the `try_call_once` error path resets the state to `Uninitialized`, another thread immediately succeeds with CAS | Immediate retry after error recovery | Initialization may be attempted repeatedly | No functional impact (CAS guarantees single execution) | 4 | Normal behavior, by design |
| F-06 | An invalid `u8` stored in `AtomicStatus` makes `Status::from_raw` hit `unreachable!` (panic) | Unsafe code corrupted by external modification | State machine logic fails | Panic instead of an invalid `Status` (no `transmute` is used) | 1 | The `AtomicStatus` wrapper prevents direct writes; `Status::from_raw` decodes via an explicit match |

## Panic Safety

- `PanicGuard` guarantees that the state is changed to `Failed` on unwind.
- The `Failed` state is terminal: all subsequent accesses panic.
- On the `try_call_once` error path: `mem::forget` is explicitly called on the `PanicGuard`
  before the state is reset to `Uninitialized`, preventing a second panic during normal error handling.
- Even if `mem::forget` were to fail (impossible, as it is safe), the guard would mark
  the cell as `Failed` on drop, which is a conservative (safe) outcome.

## Fault Management

`klazy` handles faults through the following mechanisms:

- **Error propagation**: `try_call_once` returns `Result<&T, E>`, so the caller can handle errors.
- **Poison detection**: Every access to a `Failed` state panics immediately, preventing use of inconsistent state.
- **No error codes**: The module does not return POSIX error codes; exceptions are handled through Rust's `Result` / `panic`
  mechanisms.

## Privacy Analysis

This module is a pure synchronization primitive. It does not process user data, perform I/O, or involve network communication.
It does not directly involve user privacy issues. Kernel subsystems that use this module must ensure that they manage the lifetimes of their own data correctly.

## Known Limitations

1. **Unbounded spin-wait**: `wait()` and `poll()` spin without a timeout. If the initializing thread
   is preempted indefinitely, the spinning threads waste CPU. This is acceptable in early kernel boot scenarios, but it should not be used
   with long-running initialization closures in post-boot hot paths.

2. **No priority inversion protection**: A spinning thread cannot boost the priority of the initializing thread.

3. **Poisoning is irreversible**: Once a `Once` is poisoned (`Failed`), it can never be used again.
   When `try_call_once` fails, the cell is reset to `Uninitialized` and can be retried.
   A panic, however, is irreversible.

4. **`AtomicU8` lock-free assumption**: On targets where `AtomicU8` is not lock-free,
   correctness is unaffected but performance degrades. All targets currently supported by x-kernel have
   lock-free `AtomicU8`.

## Audit Checklist

When modifying `klazy`, verify that:

- [ ] Every `unsafe` block has a `SAFETY:` comment stating the invariants.
- [ ] Every significant unsafe path can be traced back to a specific function and file location.
- [ ] Any new `u8 → Status` conversion does not bypass `AtomicStatus`.
- [ ] State transitions in `try_call_once_slow` conform to the state machine diagram in `design.md`.
- [ ] The memory ordering of any new atomic operation is no weaker than required by the table in `design.md`.
- [ ] The `Drop` implementation correctly handles all newly added states.
- [ ] `PanicGuard` covers all panic paths in new code.

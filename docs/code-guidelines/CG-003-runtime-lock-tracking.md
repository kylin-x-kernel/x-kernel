---
id: CG-003-runtime-lock-tracking
level: MUST
paths: ["**/*.rs"]
checks:
  - backend: dylint
    lint: [untracked_runtime_lock]
---

# CG-003-runtime-lock-tracking: Runtime locks must use tracked constructors

- **Rule**: Heap locks use `::new` rather than `const_new` unless an untracked const initializer is required. If a constructor is only used at runtime, drop `const` so it can keep `Mutex::new` and remain tracked. Use `Mutex::const_new(RawMutex::new(), val)` (and the RwLock equivalent) only when a `const` initializer is required and tracking is acceptable to lose.
- **Scope**: Direct `ksync::Mutex::const_new` and `ksync::RwLock::const_new` calls outside const contexts when the first argument is a direct `RawMutex::new()` or `RawRwLock::new()` call. Inline const blocks are const contexts. Closures are runtime code even when they occur within a `const` or `static` initializer. Calls using custom raw locks, including a raw lock stored in a variable or constructed with `with_config`, are outside this partial lint because replacing `const_new` with `new` would discard the raw lock configuration.
- **Why**: `Mutex::new` and `RwLock::new` use `#[track_caller]` under `stats` to record a per-initialization-site lock class. `const_new` is needed for constant initialization, but runtime uses lose that call-site tracking.
- **How to check**:
  1. Find direct calls to `ksync::Mutex::const_new` and `ksync::RwLock::const_new`.
  2. Leave calls in a const context alone, including inline const blocks. Treat a closure body as runtime code even when it appears in a `const` or `static` initializer.
  3. Report only when the raw-lock argument is a direct default `RawMutex::new()` or `RawRwLock::new()` call. Custom raw-lock values are out of scope.
  4. For a call inside a `const fn`, confirm it is actually used by a const initializer. Any exception reason must name that caller.
- **Exceptions**: Dylint allowances use the paired attributes in
  [README.md](README.md#suppressing-a-check).
  - A `const fn` constructor may use `const_new` only if it is really called from a const initializer. Suppress the call with `#[allow(unknown_lints, untracked_runtime_lock, reason = "CG-003-runtime-lock-tracking: called from <const initializer>")]`; the reason must name the caller.
  - Custom raw locks are out of scope. The lint only covers a direct default raw constructor; do not replace a custom raw lock with the default merely to satisfy this lint.
- **Counter-example**:

```rust
fn make_lock() -> ksync::Mutex<u32> {
    ksync::Mutex::const_new(ksync::RawMutex::new(), 0)
}
```

This runtime-created lock loses the per-initialization-site tracking provided by `Mutex::new`.

- **Example**:

```rust
fn make_lock() -> ksync::Mutex<u32> {
    ksync::Mutex::new(0)
}
```

- **Run**: `make dylint` scans the configured kernel; `make dylint_test` runs the lint's UI tests. Details: `tools/lints/README.md`.

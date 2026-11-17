---
id: CG-002-static-mutex-registration
level: MUST
paths: ["**/*.rs"]
checks:
  - backend: dylint
    lint: [unregistered_static_lock]
---

# CG-002-static-mutex-registration: Static mutexes must be registered through `static_lock!`

- **Rule**: Every static `ksync::Mutex` or `ksync::RwLock` must be declared through the real `static_lock!` macro, except mutexes initialized with a custom raw lock through `RawMutex::with_config(..)`. This applies with `stats` both enabled and disabled. `const_new` is for const initialization; do not use it as the default for static locks.
- **Scope**: The lint reports static and function-local static items of `ksync::Mutex` or `ksync::RwLock` type when the outer initializer, after peeling statement-free blocks, directly calls `::new(..)` or calls `const_new(Raw*::new(), ..)`. It resolves aliases and associated-type projections, and recognizes the real macro by its definition in `ksync`, `kspin` or `klockstat_macros`, not its spelling at the call site. The macro accepts only canonical `Mutex` / `RwLock` type spellings and a direct `X::new(arg)` initializer; the crate that owns the static must depend on `ksync` directly because the macro resolves that crate name from `Cargo.toml`.
- **Why**: With lock statistics enabled, `static_lock!` rewrites supported constructors to bind a per-lock statistics class. A bare static constructor does not establish that registration.
- **How to check**:
  1. Find static items whose resolved type is `ksync::Mutex` or `ksync::RwLock` and whose outer initializer, after peeling statement-free `{ .. }` and `unsafe { .. }` blocks, directly calls `::new(..)` or `const_new(Raw*::new(), ..)`.
  2. Confirm the item was produced by the real `static_lock!` macro. Renamed imports are supported; a local macro with the same name is not.
  3. Rewrite a reported item into the macro's accepted form; the diagnostic names each rewrite it needs. Spell the type as `Mutex` or `RwLock`: with `stats` the macro picks the lock kind from the written type name, so renamed imports, `type` aliases and projections do not compile. Use a bare `X::new(arg)` initializer rather than a block or `const_new`, and remove `mut` from `static mut`, which `static_lock!` rejects.
  4. For new or moved `Mutex` / `RwLock` / spin statics, also check `concurrency.md` lock construction (`static_lock!` vs `::new`).
- **Exceptions**: Dylint allowances use the paired attributes in
  [README.md](README.md#suppressing-a-check).
  - Static mutexes using a custom raw lock constructed with `RawMutex::with_config(..)` are outside this rule. `RawRwLock` has no configurable constructor.
  - Test-only locks may be exempted on the item with `#[allow(unknown_lints, unregistered_static_lock, reason = "CG-002-static-mutex-registration: <why>")]`, consistently with CG-001.
  - Ordinary non-static values should use `::new`; `static_lock!` applies to static lock items.
- **Counter-example**:

```rust
static DATA: ksync::Mutex<Vec<u8>> = ksync::Mutex::new(Vec::new());
```

The static bypasses the lock registration macro.

- **Example**:

```rust
use ksync::{Mutex, static_lock};

static_lock! {
    static DATA: Mutex<Vec<u8>> = Mutex::new(Vec::new());
}
```

- **Run**: `make dylint` scans the configured kernel; `make dylint_test` runs the lint's UI tests. Details: `tools/lints/README.md`.

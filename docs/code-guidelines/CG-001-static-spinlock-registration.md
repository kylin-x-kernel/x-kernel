---
id: CG-001-static-spinlock-registration
level: MUST
paths: ["**/*.rs"]
checks:
  - backend: dylint
    lint: [unregistered_static_spinlock]
---

# CG-001-static-spinlock-registration: Static spinlocks must be registered through `static_lock!`

- **Rule**: A `static` whose type is `kspin::SpinLock` (including the `SpinRaw`,
  `SpinNoPreempt` and `SpinNoIrq` aliases) and whose initializer is a direct
  `::new(..)` call MUST be declared inside `static_lock!`.
- **Scope**: `static` items of `kspin::SpinLock` type in all Rust code,
  including function-local statics.
- **Why**: with the `stats` feature (`KFEAT_LOCK_STAT`), `SpinLock::new`
  (`task/kspin/src/lock.rs:91-92`) stays `const` but binds the shared
  `klockstat::NOOP_CLASS`, so the lock never appears in `/proc/lock_stat`.
  `static_lock!` rewrites the initializer to `new_with_stats` with a per-lock
  class; without `stats` it is the identity macro, so builds are unchanged.
- **How to check**:
  1. Find `static` items whose resolved type is `kspin::SpinLock`; aliases,
     renamed imports and associated-type projections resolve to it.
  2. Check whether the initializer is a direct `::new(..)` call, possibly
     wrapped in statement-free `{ }` or `unsafe { }` blocks.
  3. If the item is not expanded from the real `static_lock!` macro (defined in
     `kspin`, `ksync` or `klockstat-macros`), it violates this rule. A local
     macro that merely shares the name grants no exemption.
  4. Before wrapping, rewrite the item into the form `static_lock!` accepts;
     the diagnostic names each rewrite it needs. Spell the type as the guard's
     alias `SpinRaw`, `SpinNoPreempt` or `SpinNoIrq`: with `stats` the macro
     picks the lock kind from the written type name, so renamed imports,
     `type` aliases and projections do not compile. Write the initializer as a
     bare `::new(..)` call without a `{ }` or `unsafe { }` block. Drop `mut`
     from `static mut`; the lock already provides interior mutability.
- **Exceptions**:
  - Test-only locks: test functions, `#[cfg(unittest)]` items and unit-test
    support crates. Use the paired Dylint exception attributes from
    [README.md](README.md#suppressing-a-check), with the following rule allowance:
    `#[allow(unknown_lints, unregistered_static_spinlock, reason = "CG-001-static-spinlock-registration: <why>")]`.
  - Out of scope (not reported, no suppression needed): wrapper types holding a
    lock, including per-CPU `cpu_slot_cell_with_template!` templates; locks
    built by `const fn` helpers; closures, `LazyLock`, `LazyInit`;
    `SpinRwLock` / `SpinRwNoIrq`; explicit `new_with_stats`;
    `ksync::Mutex` / `ksync::RwLock`.
  - A `SpinLock<G, T>` whose guard has no `Spin*` alias, such as `IrqSave`,
    has no `static_lock!` spelling. Prefer an aliased guard; otherwise use the
    paired exception attributes with
    `reason = "CG-001-static-spinlock-registration: no static_lock! alias for guard <G>: <why this guard>"`.
  - Macro constraints: `#[cfg(..)]` goes outside the `static_lock!`
    invocation, doc comments go inside on the item, and `static mut` is
    rejected. The crate that owns the static must depend on `kspin`
    directly: with `stats` the macro resolves that crate name from
    `Cargo.toml`, even when the type is imported through `ksync`.
- **Counter-example**:

```rust
static TABLE: kspin::SpinNoIrq<Table> = kspin::SpinNoIrq::new(Table::new());
```

`TABLE` is bound to `NOOP_CLASS` when lock statistics are enabled and never
shows up in `/proc/lock_stat`.

- **Example**:

```rust
use kspin::{SpinNoIrq, static_lock};

static_lock! {
    static TABLE: SpinNoIrq<Table> = SpinNoIrq::new(Table::new());
}
```

- **Run**: `make dylint` scans the configured kernel; `make dylint_test` runs
  the lint's UI tests. Details: `tools/lints/README.md`.

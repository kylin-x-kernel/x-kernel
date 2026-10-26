# kiface Design

`kiface` provides procedural macros for explicit kernel interface wiring.  The
first supported interface shape is an exactly-one provider boundary for
stateless, associated-function-style calls across crate layers.

## Responsibilities

- Define the `#[kiface::interface]` attribute macro: it parses the
  trait-shaped contract (`src/interface.rs`), generates the uninhabited
  facade type plus a hidden `unsafe extern "Rust"` declaration module,
  and emits the direct inherent methods callers use.
- Define the `#[kiface::provide]` attribute macro: it turns an
  inherent-impl-shaped block into exported Rust-ABI symbols
  (`src/provide.rs`) and type-checks each provider method against the
  interface facade at provider compile time.
- Own symbol naming (`src/naming.rs`) and attribute-argument parsing
  (`src/args.rs`); both sides must agree on the namespace.

Everything the generated code does at runtime is owned by the two sides
the macros wire together: the interface crate owns the contract, the
provider crate owns the behavior. This crate ships no runtime component.

## Interface Ownership and Dependency Rules

These rules govern contracts wired through `kiface`, not only the macros:

1. Group interfaces by the domain that owns the capability, not by individual
   consumers. Shared task capabilities belong to a task API contract; do not
   duplicate them as consumer-specific `MutexTaskIf` and `IrqTaskIf` contracts.
   Subsystem-specific policy is not a shared task capability merely because its
   implementation calls the scheduler.
2. API crates contain contracts and necessary boundary types only. Scheduling
   algorithms, global runtime state, and concrete implementations remain in
   their owning implementation crates. Document execution-context, lifetime,
   ownership, and failure obligations as part of each contract.
3. API crates must not depend back on their implementation crates. Consumers
   depend only on the APIs they need; a common directory or documentation entry
   must not become an umbrella dependency that pulls every API into the build.
   Providers implement the contracts without exporting internal implementation
   types through that boundary.
4. Before breaking a dependency cycle with an interface, check whether data or
   behavior belongs in a different module. Correct misplaced responsibilities
   first. Use the appropriate domain API when a genuine reverse call remains;
   a dependency cycle alone does not justify another interface.

Interface organization and binding are separate responsibilities: `kiface`
provides the wiring mechanism, while domain API crates own their contracts.
These rules do not require replacing generic libraries such as `lock_api`:
its typed lock and guard implementation is not a single-provider wiring layer.

## Domain API Index

Contract packages use `kiface-<domain>-api`; Rust paths use the corresponding
`kiface_<domain>_api` name. Both consumers and providers reference that same
contract package. The prefix distinguishes contracts from implementation
libraries, not consumers from providers. Directories remain `crates/<domain>-api`
because the parent directory already identifies kiface ownership. Concrete
synchronization libraries such as kmutex and kspin do not use this prefix.

- [`kiface-task-api`](../crates/task-api/docs/design.md): task execution and current-task
  lock diagnostics, implemented by `ktask`.
- [`kiface-task-wait-api`](../crates/task-wait-api/docs/design.md): task-bound completion
  waiting shared by IRQ and RCU, above the low-level task/preemption contracts.
- [kiface-cpu-api](../crates/cpu-api/docs/design.md): CPU identity, IRQ exclusion,
  protected exception observations and reschedule transport.
- [kiface-irq-context-api](../crates/irq-context-api/docs/design.md): IRQ/NMI/BH facts,
  provided by the IRQ core, without consumer admission policy.
- [kiface-time-api](../crates/time-api/docs/design.md): clock domains, events, idle
  repair, scoped timers and local reprogramming.
- [kiface-console-api](../crates/console-api/docs/design.md): ordinary I/O, emergency
  output and input readiness, with distinct execution-context contracts.
- [kiface-logging-api](../crates/logging-api/docs/design.md): log sink and optional
  initialization-aware context, not a mandatory current-task query.
- [kiface-mm-api](../crates/mm-api/docs/design.md): DMA mapping-attribute transitions,
  implemented by the address-space owner rather than a runtime forwarder.

The API crates are separate dependencies; the `kiface` proc-macro crate does
not depend on or reexport them.

## Specialized Contracts

Cohesive subsystem protocols remain with their owners: IRQ controller and MSI
contracts in kirq, RCU lifecycle contracts in krcu, and platform boot, power and
NMI contracts in kplat. Shared task operations do not belong in these protocols
merely because they are used internally. The API directory is not a requirement
to move every domain-specific interface out of its owning subsystem.

Synchronization libraries own lock state and algorithms; task providers own
execution and scheduling. kmutex is a concrete lock implementation, not an API
contract crate. ksync currently reexports Mutex while still implementing RwLock
and Semaphore with direct ktask dependencies; complete synchronization/task
decoupling is not established by the capability contracts alone.

## Dependencies

Build-time only: `proc-macro2`, `quote`, and `syn`. The crate is a
`proc-macro` library, so none of these reach the final kernel image; the
generated code depends on nothing beyond `core`.

## Interface Model

The definition side writes a trait-shaped contract:

```rust
#[kiface::interface]
pub trait KernelEntry {
    fn primary(boot_info: usize) -> !;
}
```

The macro expands this into an uninhabited facade type with direct inherent
methods, so callers use:

```rust
KernelEntry::primary(boot_info)
```

The provider side writes an inherent-impl-shaped block:

```rust
#[kiface::provide]
impl KernelEntry {
    fn primary(boot_info: usize) -> ! {
        rust_main(boot_info)
    }
}
```

The provider macro exports one Rust-ABI symbol per interface method and checks
the provider method signature against the generated facade method. The interface
macro declares the matching extern symbols and wraps each call in the facade
method.

Generated calls run in whatever context the caller runs in: the facade
method is a plain inline call through a function symbol with no runtime
component of its own, imposes no locking, allocation, or scheduling
assumptions, and is valid wherever both the caller and the provider body
are valid. The provider side rejects `unsafe` methods, receivers,
generics, `async`, `extern`, variadic signatures, and default bodies at
macro-expansion time (see the Current Restrictions section of
`docs/security.md`).

## Scope

The current `interface` implementation is for exactly-one, stateless wiring
points. It is intended for single-implementation cross-crate interfaces and to
avoid accidental `linkme` registries where the real contract is not one-to-many.

`#[kiface::interface(optional)]` is reserved for a future explicit optional
provider mode. The attribute argument is parsed (`src/args.rs`) but any use
today is rejected with a compile error (`src/interface.rs`); it never
silently degrades to the required-interface path. The intended user-facing
shape is to generate `try_*` methods that return `Option<R>` rather than
silently falling back to defaults. It is not implemented yet; `kiface`
should wait for stable weak-symbol support such as `extern_weak` rather
than adding a registry dependency for this single-provider case.

It intentionally does not model:

- one-to-many static registration;
- stateful opaque objects;
- dynamic runtime dispatch;
- default implementations.

Stateful object support should extend `interface` later, while registry support
should remain a separate `kiface` concept because it is one-to-many.

# kiface Design

`kiface` provides procedural macros for explicit kernel interface wiring.  The
first supported interface shape is an exactly-one provider boundary for
stateless, associated-function-style calls across crate layers.

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

## Scope

The current `interface` implementation is for exactly-one, stateless wiring
points. It is intended for single-implementation cross-crate interfaces and to
avoid accidental `linkme` registries where the real contract is not one-to-many.

`#[kiface::interface(optional)]` is reserved for a future explicit optional
provider mode. The intended user-facing shape is to generate `try_*` methods
that return `Option<R>` rather than silently falling back to defaults. It is not
implemented yet; `kiface` should wait for stable weak-symbol support such as
`extern_weak` rather than adding a registry dependency for this single-provider
case.

It intentionally does not model:

- one-to-many static registration;
- stateful opaque objects;
- dynamic runtime dispatch;
- default implementations.

Stateful object support should extend `interface` later, while registry support
should remain a separate `kiface` concept because it is one-to-many.

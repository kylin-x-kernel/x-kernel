---
id: CG-043-reuse-once-lazy-lifecycle
level: SHOULD
paths: ["**/*.rs"]
checks:
  - backend: ai
triggers:
  - '\bstatic\s+mut\b'
  - '\bAtomicPtr\b'
  - '\b(?:Once|Lazy|LazyInit)::new\s*\('
---

# CG-043-reuse-once-lazy-lifecycle: Reuse the once/lazy initialization lifecycle

- **Rule**: One-time global initialization SHOULD reuse `klazy::Once<T>`,
  `Lazy<T, F>` / `LazyInit<T>` or the supported local facade. A custom
  initialization state machine needs a documented lifecycle mismatch.
- **Scope**: Changed global initialization and its reads, including bespoke
  static-mut or atomic-pointer once-state machines outside foundational crates.
  Not this rule: the pending Dylint lint `static_mut_option` ([pending](pending.md)) owns the prohibited
  Option<T> static-mut form; the non-Option declaration candidate is in
  [pending.md](pending.md). [CG-017](CG-017-unsafe-impl-send-sync-argument.md)
  owns Send/Sync arguments. This rule owns lifecycle reuse; actual publication
  ordering defects remain concurrency review.
- **Why**: The existing once/lazy lifecycle centralizes initialization,
  publication and cleanup instead of replicating a state machine at each use.
- **How to check**:
  1. Read the changed initialization path and corresponding reads.
  2. Check whether klazy Once/Lazy/LazyInit or a supported local facade fits
     the lifecycle. Prefer it to a bespoke initialization state machine.
  3. If a custom state machine remains, require a documented lifecycle mismatch
     and distinguish that justification from proof of sound publication.
- **Exceptions**: Foundational once/lazy implementations, resettable state and
  multi-phase lifecycles may need a custom state machine; document the mismatch
  with existing one-time initialization types.
- **Counter-example**:

```rust,ignore
static GLOBAL: AtomicPtr<State> = AtomicPtr::new(core::ptr::null_mut());
// Bespoke one-time init and reads, with no explanation why Once cannot fit.
```

The custom initialization lifecycle has no documented reason to bypass Once.

- **Example**:

```rust,ignore
static GLOBAL: klazy::Once<State> = klazy::Once::new();
```

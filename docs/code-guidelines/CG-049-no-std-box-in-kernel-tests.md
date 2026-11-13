---
id: CG-049-no-std-box-in-kernel-tests
level: MUST
paths: ["**/*.rs"]
checks:
  - backend: dylint
    lint: [std_box_in_kernel_tests]
---

# CG-049-no-std-box-in-kernel-tests: Use kernel owners in kernel unit tests

- **Rule**: Kernel unit-test code MUST NOT introduce explicit standard `Box`
  ownership without a documented, narrowly scoped exception. Prefer `KBox`
  for fallible kernel ownership, stack pinning for local futures, or retaining
  an existing Vec when a boxed-slice conversion is unnecessary.
- **Scope**: Enforcement requires a compiler invocation with `--cfg unittest`.
  Within it, the lint checks compiled source with an exact `ut-tests` path
  component and source spans under outer `#[cfg(unittest)]` or `#[cfg(test)]`
  on items, associated items, expressions, local declarations, fields, and
  enum variants. Positive `all`/`any` combinations are recognized when they
  require a test condition. Adjacent production code is excluded; inner
  crate/module cfg attributes do not expand the scope. Host tests built
  without `--cfg unittest` are excluded. The collector does not infer
  test-only reachability, `cfg_attr`, negated logical equivalents, standalone
  test attributes, or the contents of test-gated out-of-line modules outside
  `ut-tests/`. Local macros are checked at their invocation; external macro
  implementation spans are excluded, while caller-preserved argument spans
  remain checked. See `tools/lints/README.md` for mechanical coverage limits.
- **Why**: Tests should exercise the kernel's intended ownership interfaces
  instead of introducing new infallible owner allocations during migration.
- **How to check**:
  1. Resolve standard Box types and constructors, including renamed imports
     and conversions such as `into_boxed_slice` and `into_boxed_str`. Merely
     importing a name is not an ownership operation. Internal allocations in
     standard macros such as `vec!` are not explicit Box use.
  2. Audit the allocation context before selecting KERNEL or NOWAIT; test
     setup is not automatically sleepable. Keep raw-pointer provenance,
     cancellation, stable-address requirements and destruction timing intact.
  3. For stack pinning, use lexical scopes when an owned future previously
     had an explicit drop boundary. Dropping a Pin reference alone does not
     destroy the future.
  4. The Dylint diagnostic is an error by default. Existing exceptions do not
     authorize new standard Box uses elsewhere in the same crate.
- **Exceptions**: Only the smallest relevant item may allow this lint, with
  the paired attributes required by the registry's exception policy. Reasons
  must identify an unmigrated production API that requires standard Box
  ownership, a raw-pointer ownership protocol that cannot yet accept KBox,
  a test specifically exercising standard Box/GlobalAlloc behavior, or an
  existing portable-driver fixture whose required heap/stable-address owner
  cannot legally depend on kernel allocation types. State that concrete
  lifetime or representation constraint, not merely "test code". Do not add
  kernel dependencies to portable driver layers, disguise a unique owner as
  a singleton Vec, or add artificial Arc sharing merely to satisfy this rule.
  Whole-crate or whole-module migration exemptions are not permitted.
  Portable drivers such as virtio must not depend on `kalloc` or
  `kalloc-types` (see
  [driver-subsystem-boundaries.md](../ai/driver-subsystem-boundaries.md)), so
  "replace with `KBox`" does not apply to their tests; use the portable-driver
  fixture exception above.
- **Counter-example**:

```rust,ignore
let fixture = Box::new(Fixture::new());
```

An ordinary test fixture does not justify an infallible standard owner.

- **Example**:

```rust,ignore
let fixture = KBox::new(Fixture::new()).expect("sleepable fixture setup");
```

This example assumes unlocked, reclaim-safe task context. Tests under locks
must instead prepare the owner earlier or select a justified NOWAIT policy.

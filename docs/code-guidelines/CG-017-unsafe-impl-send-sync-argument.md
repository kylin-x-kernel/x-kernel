---
id: CG-017-unsafe-impl-send-sync-argument
level: MUST
paths: ["**/*.rs"]
checks:
  - backend: ai
triggers:
  - "unsafe impl"
---

# CG-017-unsafe-impl-send-sync-argument: Explain the concurrency model

- **Rule**: Every `unsafe impl Send` or `unsafe impl Sync` MUST have nearby
  comments or documentation explaining the concrete ownership or concurrency
  model that makes transfer or shared access sound.
- **Scope**: Unsafe implementations of `Send` and `Sync` in Rust code,
  including generic implementations; restating the trait name is insufficient.
  Not this rule: [CG-016](CG-016-safety-comment-substance.md) owns
  safety-argument substance for other unsafe operations and impls; report a
  missing or incomplete Send/Sync concurrency argument here once, even if
  Clippy also reports its missing comment. An initialization-state defect in
  a field belongs to [CG-018](CG-018-uninitialized-storage-model.md), and a
  concrete lock-scope or atomicity bug belongs to
  [CG-013](CG-013-no-sleep-under-spinlock.md) or
  [CG-015](CG-015-no-split-critical-section.md), rather than reclassifying it
  as a second Send/Sync argument finding.
- **Why**: These implementations allow access across threads beyond what the
  fields' types establish automatically.
- **How to check**:
  1. Identify the fields and operations that need a manual thread-safety
     argument, including the implementation's generic bounds.
  2. Check that the argument names the ownership rule, synchronization, or
     confinement that permits the claimed transfer or shared access.
  3. Follow the relevant access paths to confirm that they uphold that model.
- **Exceptions**:
  1. None.
- **Counter-example**:

```rust,ignore
// SAFETY: LockedValue is thread-safe.
unsafe impl<T: Send> Sync for LockedValue<T> {}
```

The comment restates thread safety without explaining how access is synchronized.

- **Example**:

```rust,ignore
// SAFETY: LockedValue keeps its UnsafeCell<T> private and every read or write
// holds its mutex guard, including access through shared references.
// No reference to T outlives the guard; T: Send permits successive owners
// of that guard to access T on different threads.
unsafe impl<T: Send> Sync for LockedValue<T> {}
```

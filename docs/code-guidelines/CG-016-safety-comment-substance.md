---
id: CG-016-safety-comment-substance
level: MUST
paths: ["**/*.rs"]
checks:
  - backend: clippy
    lint: [undocumented_unsafe_blocks]
  - backend: ai
triggers:
  - "unsafe \\{"
  - "unsafe fn"
  - "unsafe impl"
---

# CG-016-safety-comment-substance: State the concrete safety argument

- **Rule**: Every unsafe block MUST have a preceding `// SAFETY:` justification; every unsafe impl MUST have a safety argument.
  An unsafe operation's safety argument MUST identify the operation,
  all concrete invariants it relies on, and who or which check, guard, ownership
  rule, or state transition establishes each invariant.
- **Scope**: The substance of `SAFETY:` comments and safety documentation for
  unsafe blocks, functions, and impls in Rust code. Clippy checks comment presence and placement; AI checks their substance. Relevant invariants include pointer
  validity, lifetime and allocation, alignment and bounds, initialization,
  aliasing, synchronization and execution context, pinning, immovability and
  publication ordering, and FFI ABI, ownership, callback lifetime and unwind
  assumptions. Arguments consisting only of
  "pointer is valid", "caller guarantees safety", "safe because checked above",
  "FFI call", or "this is safe by construction" violate this rule.
  Not this rule: unsafe-attribute ABI/linker justifications belong to
  the pending Dylint lint `undocumented_unsafe_attribute` ([pending](pending.md)), and Send/Sync ownership or
  concurrency arguments to [CG-017](CG-017-unsafe-impl-send-sync-argument.md);
  report those argument defects there rather than repeat them here.
  [CG-025](CG-025-minimal-unsafe-scope.md) owns unsafe-block size and
  [CG-026](CG-026-unsafe-api-safety-docs.md) owns missing `# Safety` contracts,
  while an actual initialization, user-access or output-capacity defect belongs
  to [CG-018](CG-018-uninitialized-storage-model.md),
  [CG-019](CG-019-user-pointers-stop-at-syscall-boundary.md) or
  [CG-020](CG-020-raw-output-buffer-capacity.md); this rule owns proof
  substance rather than repeating those implementation findings.
- **Why**: An explicit proof obligation makes unsafe operations auditable.
- **How to check**:
  1. Run `make clippy` with the configured platform for the mechanical checks.
  2. Identify the unsafe operation and its applicable requirements: validity,
     lifetime, alignment, initialization, exclusivity, synchronization, pinning,
     publication ordering, or FFI requirements.
  3. Match each requirement to the argument and the specific code or caller
     obligation that establishes it; name prior checks, state bits, locks,
     guards, ownership rules and current-CPU confinement explicitly. Verify
     that the code actually enforces each claim through the operation; for
     caller obligations, name the `# Safety` requirements discharged here.
     For unaligned access, explain why it is intentional and how the chosen
     operation permits it; initialization and bounds still need their own proof.
  4. Report placeholder arguments or omitted requirements, including ordering
     or publication assumptions when relevant. Check FFI ownership and
     unwind policy as well as ABI and callback lifetime; labeling a call
     "FFI" or "raw pointer" does not establish any of them.
     The [Linux Rust coding guidelines](https://docs.kernel.org/rust/coding-guidelines.html)
     also require explicit preceding SAFETY comments, including low-level code.
- **Exceptions**: No handwritten exemption. Regenerated bindgen modules may carry a focused generated-code allowance; this does not establish soundness of their consumers. Use the syntax in [README.md](README.md#suppressing-a-check).
- **Counter-example**:

```rust,ignore
let value = unsafe { pointer.read() };
```

This violates the requirement or needs a specific permitted exception.

- **Example**:

```rust,ignore
let value = 42_u32;
let pointer = &value as *const u32;
// SAFETY: pointer comes from the live, aligned, initialized u32 local;
// reading copies the value without changing the shared pointee.
let copied = unsafe { pointer.read() };
```

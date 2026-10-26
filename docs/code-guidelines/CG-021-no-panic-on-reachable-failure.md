---
id: CG-021-no-panic-on-reachable-failure
level: MUST
paths: ["**/*.rs"]
checks:
  - backend: clippy
    lint: [panic, unwrap_used, expect_used]
  - backend: ai
triggers:
  - "\\.unwrap\\(\\)"
  - "\\.expect\\("
  - "panic!"
  - "unreachable!"
  - "debug_assert!"
---

# CG-021-no-panic-on-reachable-failure: Handle legitimate failures without panicking

- **Rule**: Code MUST NOT use `.unwrap()`, `.expect()`, explicit panics, or hidden
  panic paths where failure is a legitimate possibility.
- **Scope**: Failure paths in all Rust code, including helpers reached from
  normal control flow. Determine reachability from inputs and established
  invariants; an operation proven infallible does not have a legitimate failure
  path. Runtime validation must not be downgraded to `debug_assert!`. The three Clippy lints run at warn; `make clippy` promotes warnings to errors.
  Not this rule: the pending Dylint lint `untyped_error_result` ([pending](pending.md)) owns unit/string
  `Result` error types, and
  [CG-036](CG-036-ordinary-error-propagation.md) owns ordinary `?`
  propagation style; this rule owns terminating or omitting runtime validation
  on a reachable legitimate failure. Log severity belongs to
  [CG-022](CG-022-log-level-matches-event.md), and unfinished
  `todo!`/`unimplemented!` bodies to [CG-023](CG-023-unfinished-implementations.md),
  rather than reporting those same defects again as panic findings.
- **Why**: An ordinary failure should follow the layer error contract instead of terminating the kernel.
- **How to check**:
  1. Run `make clippy` with the configured platform for the mechanical checks.
  2. Inspect each panic-capable operation and the helpers it invokes.
  3. Trace whether valid control flow, external input, or a failed operation
     can reach the panic; identify any invariant that rules it out.
  4. For a legitimate failure, check that the code handles or propagates it
     instead of panicking or relying on debug-only runtime validation.
- **Exceptions**: Per-site: a proven internal invariant; `Fatal boot: ` with its failure condition, no recovery and no error-return channel; shared test macros or `test scenario: ` helper reasons. `panic` additionally permits `TODO(cg): ` when legitimate failure cannot be reported by the current interface; state the planned interface or lifecycle repair. Do not disable boot crates or change boot interfaces for lint cleanup.
  - Registered `panic` case (b) library roots: `acpi`, `kcpu`.
  - Registered `unwrap_used` case (a): `kbuild_config` and `ksignal` build.rs only; case (b) library roots: `alloc-engine`, `kworkerpool_ng`, `virtio`, `kworkqueue_ng`, `kext4`, `ksyscall`.
  - Registered `expect_used` case (a): `kbuild_config` and `backtrace` build.rs, `unittest_support` library; case (b) library roots: `kalloc`, `khal`, `ktask`, `kworkerpool_ng`, `memspace`, `virtio`, `kworkqueue_ng`, `kvfs`, `kprocess`, `knet`, `posix-process`.
  - Case (b) threshold is ten diagnosed sites across platform configurations; only src/lib.rs is exempt. `kernel-boot` and `fs_boot` stay enabled. Use the syntax in [README.md](README.md#suppressing-a-check).
  - `debug_assert!` may check correctness invariants; it must not replace runtime validation.
- **Counter-example**:

```rust,ignore
fn read_config() -> Result<Config, Error> {
    Ok(load_config().unwrap())
}
```

This violates the requirement or needs a specific permitted exception.

- **Example**:

```rust,ignore
fn read_config() -> Result<Config, Error> {
    let config = load_config()?;
    Ok(config)
}
```

---
id: CG-021-no-panic-on-reachable-failure
level: MUST
paths: ["**/*.rs"]
checks:
  - backend: clippy
    lint: [panic, unwrap_used, expect_used]
---

# CG-021-no-panic-on-reachable-failure: Handle legitimate failures without panicking

- **Rule**: Code MUST avoid `.unwrap()`, `.expect()` and hidden panics where failure is a legitimate possibility. Return or handle errors from input, resources or environment.
- **Scope**: Normal Rust control flow and its helpers. The three lints run at warn; make clippy promotes warnings to errors.
- **Why**: An ordinary failure should follow the layer error contract instead of terminating the kernel.
- **How to check**:
  1. Run the backend with the configured platform and review each diagnostic.
  2. The lints flag syntax, not reachability. Review the specific invariant and its enforcement. Runtime validation must not become debug-only validation. Prefer local conversion to an existing Result or Option channel.
- **Exceptions**: Per-site: a proven internal invariant; `Fatal boot: ` with its failure condition, no recovery and no error-return channel; shared test macros or `test scenario: ` helper reasons. `panic` additionally permits `TODO(cg): ` when legitimate failure cannot be reported by the current interface; state the planned interface or lifecycle repair. Do not disable boot crates or change boot interfaces for lint cleanup.
  - Registered `panic` case (b) library roots: `acpi`, `kcpu`.
  - Registered `unwrap_used` case (a): `kbuild_config` and `ksignal` build.rs only; case (b) library roots: `alloc-engine`, `kworkerpool_ng`, `virtio`, `kworkqueue_ng`, `kext4`, `ksyscall`.
  - Registered `expect_used` case (a): `kbuild_config` and `backtrace` build.rs, `unittest_support` library; case (b) library roots: `kalloc`, `khal`, `ktask`, `kworkerpool_ng`, `memspace`, `virtio`, `kworkqueue_ng`, `kvfs`, `kprocess`, `knet`, `posix-process`.
  - Case (b) threshold is ten diagnosed sites across platform configurations; only src/lib.rs is exempt. `kernel-boot` and `fs_boot` stay enabled. Use the syntax in [README.md](README.md#suppressing-a-check).
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

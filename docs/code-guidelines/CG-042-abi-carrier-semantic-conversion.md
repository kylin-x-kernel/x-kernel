---
id: CG-042-abi-carrier-semantic-conversion
level: SHOULD
paths: ["**/*.rs"]
checks:
  - backend: ai
triggers:
  - '\b(?:linux_raw_sys|abi)::'
  - '#\s*\[repr\s*\(\s*(?:C|packed)\b'
---

# CG-042-abi-carrier-semantic-conversion: Convert ABI carriers into kernel semantic values

- **Rule**: Code SHOULD separate external ABI carrier types from validated
  in-kernel semantic types and convert as early as practical at the boundary,
  instead of passing raw ABI wrappers deeper when a semantic value can be formed.
- **Scope**: External value layout, flags and encoding carriers at kernel
  boundaries and the first subsystem helper that consumes them.
  Not this rule: [CG-019](CG-019-user-pointers-stop-at-syscall-boundary.md)
  owns user addresses and syscall access placement; the pending Dylint lint `unsound_user_abi_impl` ([pending](pending.md))
  and the pending Dylint lint `user_copy_without_abi_contract` ([pending](pending.md)) own user-copy layout and
  typed-copy validity. This rule owns value-carrier versus semantic-type
  separation beyond those pointer/copy contracts.
- **Why**: External encoding conventions should not become implicit kernel
  invariants interpreted differently by successive subsystem layers.
- **How to check**:
  1. Identify the changed external carrier and its ABI encoding obligations.
  2. Read the boundary conversion and first helper; check that validation and
     conversion produce the kernel semantic value as early as practical.
  3. Report a raw carrier passed deeper where a semantic type can be formed,
     rather than demanding changes to necessary ABI storage or parsing.
- **Exceptions**: ABI-preserving parsing, serialization and actual ABI storage may
  retain carriers where their external representation is the purpose.
- **Counter-example**:

```rust,ignore
fn sys_set_mode(raw: abi::Mode) -> Result<()> {
    subsystem::set_mode(raw)
}
```

The first subsystem helper receives an external carrier despite an available validated Mode.

- **Example**:

```rust,ignore
fn sys_set_mode(raw: abi::Mode) -> Result<()> {
    let mode = Mode::try_from(raw)?;
    subsystem::set_mode(mode)
}
```

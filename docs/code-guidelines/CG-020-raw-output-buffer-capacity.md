---
id: CG-020-raw-output-buffer-capacity
level: MUST
paths: ["**/*.rs"]
checks:
  - backend: ai
triggers:
  - "write_unaligned"
  - "copy_nonoverlapping"
  - "write_bytes"
  - "\\.add\\("
  - "\\.offset\\("
---

# CG-020-raw-output-buffer-capacity: Prove capacity before raw writes

- **Rule**: Before each raw write or cursor advance through a fixed-capacity
  output buffer, code MUST prove that the destination byte range fits the
  buffer and that the size or offset calculation cannot overflow.
- **Scope**: Raw-pointer serialization and structured output into fixed-capacity
  buffers in Rust code, including bootloader buffers. A later final length
  field or the fact that a page was allocated does not prove earlier writes fit.
  Not this rule: [CG-018](CG-018-uninitialized-storage-model.md) owns
  constructing valid initialized values,
  the pending Dylint lint `unsound_user_abi_impl` ([pending](pending.md)) and
  the pending Dylint lint `user_copy_without_abi_contract` ([pending](pending.md)) own user-copy representation
  contracts, and [CG-019](CG-019-user-pointers-stop-at-syscall-boundary.md)
  owns syscall user-access placement; this rule owns destination capacity and
  arithmetic overflow in raw output serialization. A missing comment for a
  capacity check that already proves the write safe belongs to
  [CG-016](CG-016-safety-comment-substance.md).
- **Why**: A raw pointer carries no remaining capacity, and a write can corrupt
  memory before a final size calculation detects the overrun.
- **How to check**:
  1. Identify the buffer capacity and the typed size calculation or cursor
     offset used to establish each destination byte range.
  2. Verify overflow-safe arithmetic, such as `checked_add`, and capacity
     checks before advancing the cursor or performing each write.
  3. Confirm that the proof covers the full byte count of `write`,
     `write_unaligned`, `copy_nonoverlapping`, or `write_bytes` on every path.
- **Exceptions**:
  1. None.
- **Counter-example**:

```rust,ignore
let cursor = output.as_mut_ptr();
// SAFETY: the bootloader allocated this output page.
unsafe { cursor.cast::<Header>().write_unaligned(header) };
```

An allocated page alone does not establish that this header fits the output buffer.

- **Example**:

```rust,ignore
let bytes = header_word.to_ne_bytes();
let end = offset.checked_add(bytes.len()).ok_or(Error::BufferTooSmall)?;
if end > output.len() {
    return Err(Error::BufferTooSmall);
}
// SAFETY: checked_add and the capacity check prove the full destination
// range lies in the exclusively borrowed output slice; the initialized local
// bytes array is separate from output, and u8 requires no extra alignment.
unsafe {
    core::ptr::copy_nonoverlapping(
        bytes.as_ptr(), output.as_mut_ptr().add(offset), bytes.len(),
    );
}
offset = end;
```

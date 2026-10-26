---
id: CG-045-non-time-integer-units
level: SHOULD
paths: ["**/*.rs"]
checks:
  - backend: ai
triggers:
  - '\b(?:size|len|length|offset|capacity)\s*:\s*(?:u|i)(?:8|16|32|64|128|size)\b'
  - '\blet(?:\s+mut)?\s+(?:size|len|length|offset|capacity)\s*[:=]'
---

# CG-045-non-time-integer-units: Make non-time integer units explicit

- **Rule**: Non-time integer quantities SHOULD name their byte, page or
  element units when neither the type nor an unmistakable local contract
  supplies them.
- **Scope**: Integer sizes, lengths, offsets and capacities with ambiguous
  non-time units. Initial triggers cover only the measured bare size/len/length/
  offset/capacity declarations and bindings.
  Not this rule: the pending Dylint lint `unitless_time_integer` ([pending](pending.md)) owns integer time-unit
  suffixes. Descriptive semantic names and names that reveal scans or side
  effects remain deferred guidance in
  [naming-and-structure.md](../ai/skills/code-guidelines/naming-and-structure.md).
  This rule owns ambiguity between non-time units, not general naming taste.
- **Why**: An unqualified integer can silently mix bytes, pages and element
  counts across producer and consumer boundaries.
- **How to check**:
  1. Read the quantity producer and first consumers to identify the stored unit.
  2. Determine whether its type or unmistakable local contract already supplies
     that unit. Do not request suffixes solely because a trigger matched.
  3. For a real ambiguity, use a unit-bearing type or a name such as size_pages
     rather than size, offset_bytes or capacity_entries as appropriate.
- **Exceptions**: Ordinary unambiguous slice lengths, indices and ABI-mandated
  names need no suffix. An unmistakable local contract or unit-bearing type
  may supply the unit instead of repeating it in the identifier.
- **Counter-example**:

```rust,ignore
fn map_pages(size: usize) -> Result<()> {
    reserve_bytes(size)
}
```

The producer and consumer leave it unclear whether size counts pages or bytes.

- **Example**:

```rust,ignore
fn map_pages(size_pages: usize) -> Result<()> {
    let size_bytes = size_pages.checked_mul(PAGE_SIZE).ok_or(Error::Overflow)?;
    reserve_bytes(size_bytes)
}
```

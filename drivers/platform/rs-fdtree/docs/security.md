# rs_fdtree — Security And Reliability

## DMA declaration boundary

The allocation-free DMA iterator checks complete tuple shape and both inclusive
window ends; a valid range may end at u64::MAX. Consumers must reject any error
rather than use a partial window set. Coherency markers must be empty booleans
and cannot conflict; node-local duplicate properties fail strict lookup.
Neither a range nor a missing property authorizes DMA, supplies a device mask,
or implies host support for multiple windows/IOMMU routing.

## Strict property trust boundary

Strict readers reject duplicate or malformed properties before returning values.
Parent traversal uses the validated immutable tree; offsets are only meaningful
within that tree. These operations neither allocate nor interpret status or
address bindings. The separate address module validates translated resources.

## Translated address input boundary

The address module rejects malformed/duplicate properties, invalid reg tuples,
reg-names, unsupported cell widths, zero lengths and arithmetic overflow.
Containment checks cover the full interval; missing ranges is not identity.
After rejecting zero size, bounds use base + size - 1, including 32-bit widths.
A last byte at u64::MAX is valid. Subtraction-based containment never needs an
unrepresentable exclusive end. Metadata collection requires an allocator and
follows its existing OOM policy. It introduces no mappings or hardware access;
returned names borrow the immutable input tree.

## Phandle references and interrupt specifiers

`LinuxFdt::node_by_phandle`, `FdtNode::phandle`, `interrupt_parent`,
`reference` and `irq_spec` are strict interpretations of validated
properties. Duplicate or malformed properties are errors; zero and `u32::MAX`
name no node. Interrupt specifiers keep controller cells raw and apply no
GIC/PLIC policy. `reference` checks the requested stride with
`checked_add`/`checked_mul`, rejects a property length that is not a whole
number of entries, and bounds the argument slice with `Cells::slice`;
`irq_spec` rejects zero or unrepresentable `#interrupt-cells` and truncated
`interrupts-extended` entries. Resolving a dangling phandle is
`UnresolvedPhandle`, not silence. The plain accessors rescans the validated
structure block per lookup; they do not trust offsets supplied by callers.

`NodeIndex` is the only indexed implementation and requires the allocator.
It records `NodeParts` (tree, offset, cached effective `interrupt-parent`)
for every node in one traversal; it stores no raw pointers. Offset lookup
uses a binary search over depth-first order, which is strictly increasing
structure-offset order. Phandle entries are sorted and first-wins deduplicated
in firmware order, matching the allocation-free search. A malformed or
duplicate `phandle` property is ignored by the index exactly as the linear
lookup ignores it. Allocation failure follows the allocator's existing policy;
the index adds no mappings or hardware access.

## Scope

This analysis covers the entire crate: `src/lib.rs` (`LinuxFdt`,
`MemReserveIter`), `src/header.rs`, `src/parsing.rs`, `src/node.rs`,
`src/property.rs`, `src/reference.rs`, `src/index.rs`, `src/address.rs`,
`src/dma.rs`,
`src/fwnode_backend/`, `src/error.rs`, and `src/kernel_nodes/`. No modules
are excluded; everything is reachable from `LinuxFdt::new` /
`LinuxFdt::from_ptr` and the iterators they produce. `DtBackend` is an
optional `fwnode`-feature adapter over the same validated tree; it performs
no additional parsing.


### `Chosen::x_kernel_log_level`

`x_kernel_log_level()` forwards the string value of the vendor property
`x-kernel,log-level` from the validated blob, with the same guarantees as every
other `property_str` accessor: the value is bounds-checked against the blob, must
be NUL-terminated, and a missing or non-string property yields `None`. The string
is returned unvalidated on purpose — the logging subsystem owns the level
vocabulary and rejects what it cannot parse.

## Trust Model

The DTB blob is external, untrusted input until validated. This crate is
the structural validation gate for the whole device-tree stack: once
`LinuxFdt::new` accepts a blob, its container ranges, reservation terminator,
node tokens, property headers, name offsets, UTF-8 names, and payload ranges
have been checked. Callers (`of`, boot code) must keep the blob alive and
immutable as long as borrowed views exist.

## External Boundaries

- `LinuxFdt::new(&[u8])` — safe entry: validates header magic
  (`0xd00dfeed`), clamps the working slice to the declared `totalsize`,
  and validates all container and structural data before publishing views.
- `LinuxFdt::from_ptr(*const u8)` — unsafe boot entry: reads a header-
  sized view, then the header-declared total size. The precondition (see
  Unsafe Code) covers pointer validity and lifetime.
- All property/node bytes handed upward are slices into the validated
  blob. Generic phandle/interrupt-specifier interpretation is bounded here;
  controller-specific meaning remains with the consumers (`of` today, the
  controller drivers with `irq_domain` later).

## Unsafe Code

One unsafe function: `LinuxFdt::from_ptr`, containing two
`slice::from_raw_parts` calls with `# Safety` documentation:

1. header-sized view (`size_of::<FdtHeader>()` bytes) — used to read
   `totalsize`;
2. full view (`totalsize` bytes) — handed to the safe `new` for the complete structural validation.

Preconditions: `ptr` is non-null (checked, `FdtError::BadPtr`), points to
a readable FDT, and the header plus total-size range remain accessible and immutable
for the returned lifetime. The safe slice constructor rejects a blob shorter than its declared
`totalsize`; the raw-pointer constructor cannot discover the allocation length.
The memory-safety burden for those bytes stays with its caller because the
slice is formed before validation.

## Invariants

- No read ever leaves the clamped blob slice: header offsets select
  sub-slices of `&data[..totalsize]`, and the parsing cursor returns
  `None` instead of indexing past a block.
- Iterators terminate: node walking stops at the structure-block end
  token; `MemReserveIter` stops at the `0,0` terminator pair; every
  scalar read checks remaining length first.
- Complete properties and NUL-terminated UTF-8 names are constructor invariants;
  traversal assertions do not accept unchecked firmware data.
- Nesting is at most 63 nodes, including the root, keeping traversal within its
  fixed parent array and bounding recursive lookup stack use.
- The crate keeps no global state; safety never depends on call ordering.

## Threat Analysis

| ID | Threat | Impact | Trigger | Existing control |
|----|--------|--------|---------|------------------|
| T-01 | `from_ptr` on a short or unmapped buffer | High — out-of-bounds read while reading the header/size | Boot pointer to truncated or freed memory | Owned by the caller via the `# Safety` contract; the crate cannot probe length safely. Residual risk accepted at the boot boundary. |
| T-02 | Malformed block ranges, node tokens, or properties | Rejected tree | Corrupted firmware input | Construction returns `ParseError` before exposing traversal; semantic hardware claims remain caller-owned. |
| T-03 | Declared `totalsize` exceeding the real blob | High — OOB slice formation | Forged header | `new` checks `data.len() >= totalsize` and rejects (`BufferTooSmall`); for `from_ptr` the caller owns the real length. |
| T-04 | Infinite loop on a malformed node stream | Medium — boot hang | Structure block without an end token | Validation requires balanced delimiters, a terminal `FDT_END`, bounded depth, and progress within the declared structure range. |
| T-05 | Phandle or interrupt specifier arithmetic overflows | Low — wrong reference or rejected lookup | Hostile `#interrupt-cells`, huge indices, or a truncated `interrupts-extended` list | `checked_mul`/`checked_add`, exact property-length divisibility, and `Cells::slice` bounds; failures are `ReferenceError`, never indexing outside the property. |
| T-06 | Indexed lookup disagrees with the linear lookup | Medium — device resolves to the wrong node | Duplicate phandles or same-offset entries | Stable sort plus first-wins dedup mirrors firmware-order linear lookup; offsets are collected from the validated traversal, never from firmware input. |

## Failure Modes

| ID | Failure mode | Local effect | System effect | Severity | Handling |
|----|--------------|--------------|---------------|----------|----------|
| F-01 | Bad magic | `FdtError::BadMagic` | Caller aborts DTB path | 3 | Explicit error at construction. |
| F-02 | Buffer smaller than declared size | `FdtError::BufferTooSmall` | Caller aborts or re-slices | 3 | Size check before slicing. |
| F-03 | Malformed structure, invalid range/name, or excessive nesting | `FdtError::ParseError` | Caller rejects DTB before lookup | 3 | Complete constructor validation; corruption is distinct from absence. |

Host regressions cover invalid tokens, property headers, name offsets and UTF-8,
block bounds and overlap, reservation termination, nesting limits, truncations,
raw-pointer validation, and single-byte mutations followed by traversal.

## Known Limitations

- The crate validates structure, not semantics: it cannot detect a
  well-formed tree describing wrong hardware.
- Padding bytes are bounded but their contents are ignored for compatibility
  with the existing crosvm fixture. This is structural traversal validation, not
  complete device-binding or reserved physical-region semantic validation.
- `from_ptr` necessarily trusts the header before it can validate it; the
  initial header-sized read is the irreducible trusted step.

## Audit Checklist

- Any new block offset math stays within `&data[..totalsize]`.
- New iterators keep the "check length, then read" discipline and
  terminate on malformed streams.
- `from_ptr`'s `# Safety` text still matches the two slice formations.
- New phandle/specifier accessors use checked arithmetic and treat a dangling
  phandle as an error, not as node zero.
- New allocation-backed helpers build from the validated traversal and keep
  their lookup order identical to the allocation-free path.

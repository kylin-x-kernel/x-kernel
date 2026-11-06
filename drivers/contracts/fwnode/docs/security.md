# fwnode — Security And Reliability

## Trust Model

Firmware data is external and untrusted until a backend validates it. This
crate contains no parser: it defines types and an object-safe trait. The
security boundary is therefore split:

- the **backend** owns validation of the firmware representation and is
  trusted in-kernel code;
- the **contract** owns representation-independent checks: opaque IDs,
  borrowed lifetimes, fixed-capacity buffers and string/integer conversions;
- the **consumer** owns policy, such as whether a missing resource is fatal.

A `FwNode` handle is not a capability. `FwNode::new` is public so backends can
construct handles, which means consumers could in principle manufacture an
arbitrary `FwNodeId`. Every backend method must treat an unknown ID as
`None` or `FwError::InvalidNode`; no backend may index storage directly from
an unchecked ID. `FwNodeId::raw()` exposes the encoding for diagnostics and
for passing it back to the same backend, not for dereferencing.

## Scope

The analysis covers `src/lib.rs` and the host tests in `tests/property.rs`.
The crate has no `unsafe` code, no allocation and no dependencies, so there is
no external-input parsing surface inside it.

## External Boundaries

- Every `FwNodeOps` implementation is an external boundary for the trait
  contract: it receives caller-provided names, indices and `arg_cells`, and it
  returns borrowed firmware data. The contract constrains index/capacity
  handling but cannot validate the backend's firmware semantics.
- `FwParams::new(&[u32])` is the only slice-to-capacity conversion. It rejects
  more than `FW_IRQ_SPEC_MAX_PARAMS` cells with `BufferTooSmall` before any
  copy.
- `FwProperty::as_str` converts backend bytes to UTF-8, stopping at the first
  NUL or at the end of the value.
- `FwProperty::as_u32` / `as_u64` accept only exact-width byte cells or typed
  values; every other encoding is `InvalidValue`.

## Protected Invariants

- `FwParams::len` is a `u8` but is only ever assigned from a slice length that
  has already been checked against 16, so it can never exceed the backing
  array. `as_slice` indexes `&data[..len]` with that value.
- `as_u32`/`as_u64` require exact byte lengths before indexing the byte array.
- `as_str` bounds the scan to the value slice; no NUL is required, and invalid
  UTF-8 is rejected rather than replaced or truncated silently.
- `as_bool` returns `InvalidValue` for non-flag, non-numeric values instead of
  guessing.
- `FwNode::descendants` uses `next_dfs`, which terminates at the root's
  last descendant. It relies on a well-behaved backend: a malformed
  `next_sibling`/`parent` relation could make the iterator loop or escape the
  intended subtree, but cannot cause memory unsafety or out-of-bounds access
  inside this crate.
- `FwNode::firmware_ids` increments its index until the backend returns
  `None`; a backend that never terminates its identity list can make the
  iterator run indefinitely. Backends must return `None` past the last entry.

## Threat Analysis

| ID | Threat | Impact | Trigger | Existing control |
|----|--------|--------|---------|------------------|
| T-01 | Consumer fabricates an arbitrary `FwNodeId` | Low — wrong node or backend error | `FwNode::new` is public | Backend methods map unknown IDs to `None`/`InvalidNode`; the ID is opaque and never dereferenced by the contract. |
| T-02 | Oversized parameter list | Low — rejected request | A binding asks for more than 16 cells | `FwParams::new`/`BufferTooSmall`; backend `reference` pre-checks `arg_cells`. |
| T-03 | Malformed property converted silently | Medium — wrong configuration | Truncated or wrongly typed firmware value | Exact-width integer checks; UTF-8 validation; `InvalidValue` instead of a guess. |
| T-04 | Backend returns data with an invalid handle | Low — missing resource or error | Backend bug or corrupt firmware | The contract does not index; the backend must validate IDs before use. |
| T-05 | Non-terminating traversal or identity list | Medium — CPU hang in a consumer | Backend violates parent/sibling or `firmware_id` termination | Documented backend obligation; no in-contract bound, because a valid tree size is backend-specific. |
| T-06 | Handle use after the backend is dropped | High — dangling borrow, compile-time prevented | Caller stores a `FwNode` beyond the backend lifetime | `FwNode<'a>` borrows `&'a dyn FwNodeOps<'a>`; the Rust borrow checker enforces the lifetime. |

## Failure Modes

| ID | Failure mode | Local effect | System effect | Severity | Handling |
|----|--------------|--------------|---------------|----------|----------|
| F-01 | Unknown node handle | `None` / `InvalidNode` | Device or resource skipped | 3 | Explicit backend result; no panic. |
| F-02 | Absent property or resource | `Ok(None)` | Caller applies its own default or skips | 3 | Absence is distinct from corruption. |
| F-03 | Malformed value | `InvalidValue` | Caller rejects the node/resource | 3 | No partial interpretation. |
| F-04 | Unsupported encoding | `Unsupported` | Caller falls back or reports unsupported hardware | 3 | Backend-specific capability gap. |
| F-05 | Capacity exceeded | `BufferTooSmall` | Caller skips the specifier | 3 | Checked before copying. |

## Known Limitations

- The contract cannot detect a backend that returns well-formed but wrong
  facts; that is the firmware trust boundary owned by the backend and the
  platform policy above it.
- Firmware identity strings are returned verbatim. Driver matching treats them
  as untrusted comparison keys, not as executable configuration.
- The lifetime of firmware-owned bytes is the backend's responsibility; this
  crate only propagates it.

## Audit Checklist

- No `unsafe`, no allocation, no panics added on firmware-controlled input.
- Every new fixed-capacity field checks capacity before copying and stores a
  length that cannot exceed its array.
- New accessors reject ambiguous or wrong-width encodings instead of guessing.
- Trait methods still document invalid-handle and malformed-input behavior for
  backends to implement.

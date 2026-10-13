# display — Security And Reliability

## Scope

This analysis covers the entire crate — `src/lib.rs` (the
`DisplayDevice` trait and scanout value types) plus `ut-tests/`,
which is test-only. The crate holds no state and no unsafe code; the
security-relevant content is the scanout resource contract and the
obligations it places on callers and drivers.


## Trust Model

Pure contract layer: one trait plus value types, no state, no I/O. The
security-relevant content is the scanout resource contract — guest
physical addresses handed to a host — and the obligations placed on
implementers, recorded below for driver audits.

## External Boundaries

- `create_scanout_resource(resource, paddr, length)` hands a guest
  physical address and length to the display driver, which describes it
  to the host (e.g. virtio-gpu). The kernel asserts the region is valid
  at the call site; the host becomes a reader/writer of that memory for
  the resource lifetime. This is the crate's one real trust boundary and
  it is owned by callers and drivers, not by this crate.
- No user-space input reaches this crate.

## Unsafe Code

None. The crate contains no `unsafe`, no FFI, and no inline assembly.

## Invariants

- `ScanoutRect` coordinates are plain data; drivers must clamp or reject
  rectangles outside the resource before programming hardware.
- A scanout resource id is driver-scoped; `destroy_scanout_resource` on
  an unknown id returns an error rather than affecting other resources.
- `flush` is repeatable and side-effect-free beyond presenting pixels.

## Threat Analysis

| ID | Threat | Impact | Trigger | Existing control |
|----|--------|--------|---------|------------------|
| T-01 | Guest memory described to the host while unmapped or reused | High — device writes into unrelated kernel memory | Caller frees/reuses the backing page while the resource lives | Contract: the caller keeps the backing alive for the resource lifetime (fbdevice holds its `GlobalPage` forever); enforcement by driver/caller review. Core residual risk of the scanout model. |
| T-02 | Driver programming hardware with an out-of-bounds rect | Medium — scanout reads outside the resource | Malformed `ScanoutRect` | Trait-level obligation: drivers must validate rects; the contract type cannot enforce it. |
| T-03 | Host reads stale kernel data from the shared buffer | Medium — kernel data disclosure through the display | Buffer not zeroed before binding | Callers zero the shadow before publishing (fbdevice does); recorded as a caller obligation. |

## Failure Modes

| ID | Failure mode | Local effect | System effect | Severity | Handling |
|----|--------------|--------------|---------------|----------|----------|
| F-01 | Scanout capability absent | `DriverError::Unsupported` from the default methods | Callers fall back to plain flush | 4 | Default trait implementations. |
| F-02 | Flush hardware failure | `DriverError` returned | Frame not presented; retry by caller | 3 | Error propagation via `DriverResult`. |
| F-03 | Unknown resource id on destroy/present | Driver-specific error | No effect on other resources | 4 | Driver returns an error; no implicit cleanup. |

## Known Limitations

- Single pixel format (`Bgra8888`); format negotiation does not exist.
- The contract cannot express resource lifetime to the host — that is
  caller discipline, documented here and in implementer crates.

## Audit Checklist

- Drivers validate `ScanoutRect` against resource bounds before hardware
  programming.
- New trait methods that hand memory to a host document the required
  lifetime discipline.

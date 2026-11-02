# ktask-types security and reliability

## Trust model

`ktask-types` holds no runtime state and no allocation. Callers construct
`OwnerKey` values only at the token/address boundary; the constructors reject
inputs that would break the aligned-key invariant.

## External boundaries / attack surface

There is no user memory, device, network, filesystem, or hardware access. The
only inputs are the dense token and the raw aligned word, which enter through
the checked constructors.

## Memory-safety invariants

No `unsafe` code. `OwnerKey` is `#[repr(transparent)]` over `NonZeroU64`, which
mechanically excludes the unlocked zero sentinel; the low `FLAG_BITS` bits are
kept clear by construction.

## Threats and failure modes

| Trigger | Effect | Existing control |
| --- | --- | --- |
| Zero token | Unlock sentinel aliasing a task | `from_token(0)` returns `None`. |
| Overflowing token | Silent wrap to zero | `from_token` checks the upper bound. |
| Raw key with set flag bits | Flag packing aliases another task | `from_aligned` rejects nonzero low bits. |

## Audit checklist

- New constructors enforce the aligned-key invariant rather than documenting it.
- Raw-value access goes through `as_raw` at a boundary that owns the flag bits.
- No `0` value is representable as an `OwnerKey`.
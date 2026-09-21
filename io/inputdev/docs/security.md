# inputdev — Security and reliability

## Analysis scope

This analysis covers the whole crate, which currently consists of
`src/lib.rs`. No source module or test-only path is excluded.

## Trust model

The crate receives class handles and removal ids from trusted kernel driver
infrastructure. `kdevice` and `kclass` create and validate those handles and
ids; `inputdev` trusts that contract and does not independently revalidate
their provenance or internal identity. Its local validation is limited to
deduplicating registration by id and removing the matching lifecycle entry.
It has no user-memory, MMIO, DMA, network, or filesystem boundary and contains
no unsafe code.

## Invariants and thread safety

- At most one stored handle has a given driver-core id.
- Every registry mutation is serialized by the same mutex.
- Snapshot callers receive cloned handles; reading the registry does not alter
  availability for another caller.
- Removal of an absent id is a no-op.

## Threat analysis

| ID | Threat | Impact | Trigger | Existing control |
|----|--------|--------|---------|------------------|
| T-01 | Duplicate callbacks grow the registry | Low — unbounded registry memory growth | Probe notification repeats | Deduplicate by id under the registry lock. |
| T-02 | One consumer hides devices from another | Medium — peer consumers omit live devices | A destructive registry read | `input_devices` returns a clone and preserves the registry. |
| T-03 | Consumer retains a removed handle | Medium — stale operations can outlive hot-unplug | Hot-unplug races a snapshot | Accepted by design: driver-core availability remains authoritative for post-removal semantics, and consumers must return `ENODEV` after removal. |

Retaining a class handle across removal is an explicitly accepted residual
risk. `inputdev` cannot invalidate cloned handles already owned by consumers;
the driver core remains responsible for making the removed state authoritative,
and each consumer must translate that state into its post-removal error
semantics.

## Failure modes and limitations

An empty snapshot is valid. Handle order is unstable after removal. There is no
blocking wait API; consumers that need change notification subscribe at the
class or devfs layer.

## Audit checklist

- Keep registration deduplication and mutation under one lock.
- Do not reintroduce destructive drain semantics.
- Keep slow consumer work outside the registry lock.

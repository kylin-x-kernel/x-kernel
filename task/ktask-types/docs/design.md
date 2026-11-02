# ktask-types design

## Purpose and scope

`ktask-types` owns the task owner key type shared by the lock-word producer
(`ktask`) and consumers (`kmutex`). It is a pure value-type crate with no
scheduler, registry, or runtime state; its only non-core dependency is the
`unittest` harness, pulled in only for `cfg(unittest)` tests.

## Architecture

- `src/lib.rs`: `OwnerKey` and its `FLAG_BITS` / `FLAG_MASK` / `ALIGN`
  constants plus the `from_token` / `from_aligned` constructors.

`OwnerKey` is an identity token left-shifted by `FLAG_BITS`, so the low three
bits are zero. Lock words OR `WAITERS` / `HANDOFF` / `PICKUP`-style flags into
those bits without aliasing another task's key. The type is
`#[repr(transparent)]` over `NonZeroU64`, so `0` remains the unlocked word.

## State and algorithm

The constructors enforce the invariant at the boundary:

- `from_token` rejects `0` and any token whose aligned projection overflows
  (`token > u64::MAX >> FLAG_BITS`), instead of silently wrapping to zero.
- `from_aligned` rejects `0` and any raw value with a set low flag bit.

The encode/drop of the low bits is equality-preserving: within the aligned
domain, distinct tokens stay distinct and the shifted value is an
equality-preserving relabel of the dense token.

## Design decisions

The bit-encoding rule lives here rather than in `kiface-task-api` because it is
a task identity concern shared by the producer and consumers, not a capability
contract. `kiface-task-api` depends on this crate for the `OwnerKey` boundary
type, following the same pattern as `kiface-time-api` depending on
`ktime_types` for `TimeSpan` and `Frequency`.
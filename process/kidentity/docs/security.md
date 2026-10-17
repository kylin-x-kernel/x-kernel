# kidentity security and reliability

## Scope, assets, and boundaries

The whole crate (`src/lib.rs`) is covered. Assets are stable task identities,
namespace ancestry, numeric projections consumed by process registries, and the
per-identity lifecycle state. Trusted callers supply namespace handles and fixed
root numbers. There is no direct user-pointer, filesystem, network, MMIO, DMA,
firmware, FFI, or assembly input. `kprocess`/task lifecycle owners are
responsible for authorization and publication before runnable state.

## Unsafe inventory and invariants

There is no unsafe code. Every `Upid` owns a live namespace reference. Public
constructors create an acyclic parent chain and a root projection in every
handle. `root_nr` relies on that projection. Namespace identity comparison is
pointer-based except for the documented root fallback. A fixed number is not a
unique allocation guarantee. `visible_number` resolves only through the
identity's own ancestor projections, so an out-of-hierarchy observer cannot be
confused into a foreign number.

## Thread safety

`AtomicU32` serializes per-namespace allocation; the lifecycle byte is an
`AtomicU8` guarded by compare-exchange transitions. Immutable `Arc` graphs and
vectors need no further internal locks. Allocation across several namespaces is
not an atomic transaction: a failure leaves earlier counter increments consumed.
`LazyInit` publishes the global root once, independently of task publication.

## Threat analysis

| ID | Threat and asset | Severity | Trigger | Response and residual risk |
|---|---|---|---|---|
| T-01 | Identity exhaustion | Medium | Repeated allocations reach the counter bound | Checked increment returns `WouldBlock`; no wrap, reuse, rollback, or reclamation exists. |
| T-02 | Duplicate numeric registration | High | Trusted caller uses `fixed_root` for an already allocated number | Fixed projections are explicitly unchecked; registry owners must enforce uniqueness and stable identity, not rely on this constructor. |
| T-03 | Wrong namespace visibility decision | High | Caller interprets root fallback as namespace equality | `nr_in` documents fallback for any root; non-root misses return `None`. Authorization must use the actual namespace policy. |
| T-04 | Unstable visible task identity | Medium | Upper layer makes task runnable before publication | Upper-layer lifecycle must publish first; this allocator does not implement that transaction. |
| T-05 | Excessive namespace nesting | Medium | Trusted callers repeatedly construct children | No depth cap is enforced; allocation/lookup cost and parent-drop depth increase, and `level + 1` can overflow. Callers must bound nesting. |
| T-06 | Half-published or recycling identity discovered | High | Upper layer ignores `is_lookup_visible` | Lookup helpers must filter on lifecycle state; the state machine rejects illegal transitions and `Reserved`/`Reclaimed` are never lookup-visible. |
| T-07 | Lifecycle transition race | Medium | Two owners transition the same identity concurrently | Compare-exchange applies one transition; the looser of conflicting owners sees `false` and aborts. Owner agreement on who drives transitions remains an upper-layer invariant. |

## Failure modes and effects (FMEA)

| ID | Failure mode | Cause | Local effect | System effect | Severity (1-4) | Handling |
|---|---|---|---|---|---|---|
| F-01 | Number exhaustion | Counter at `u32::MAX` | `WouldBlock` | New task creation fails | 3 | Propagate error; consumed descendant numbers are retained. |
| F-02 | Missing root projection | Broken internal constructor invariant | `root_nr` panics | Kernel service fails | 2 | Constructors always append root or create a root-only handle. |
| F-03 | Premature root allocation | Linux-visible task created before init | First number consumed | Init's PID-1 check can fail | 2 | Boot lifecycle controls allocation order. |
| F-04 | Allocation/depth failure | Heap exhaustion or excessive nesting | Allocation failure or overflow behavior | Process creation unavailable | 2 | External resource/depth policy; no local recovery. |
| F-05 | Illegal lifecycle transition attempted | Upper layer logic error or concurrency | Split transition returns `false` | Caller sees a failed state change | 3 | Transition methods are total (no panic); callers treat `false` as an invariant violation. |

## Failure handling, privacy, and limitations

Recoverable number failures use `KResult`; namespace misses use `Option`.
Lifecycle transitions are total and never panic. The global-root `unwrap` relies
on completed `call_once`. No retry or reclaim exists. The crate stores numeric
process metadata, namespace relationships, and lifecycle state, not user
payload, and writes no logs. Upper layers must authorize exposing that metadata.
Lack of reuse, linear namespace lookup, unchecked fixed projections, root
fallback, and lifecycle transitions not yet driven by task owners are
deliberate/current constraints. `is_lookup_visible` is safe to consult but no
in-tree lookup consumes it yet; until kprocess publication/exit drives
`mark_*`, identities stay in their constructed state.

## Audit checklist

- Keep checked per-namespace increments and root projections.
- Verify external uniqueness for `fixed_root` callers.
- Preserve publish-before-runnable ordering in task owners.
- Do not conflate root fallback with pointer identity or authorization.
- Filter lookups with `is_lookup_visible` once task owners drive transitions.
- Reassess retained identities before introducing PID reuse.

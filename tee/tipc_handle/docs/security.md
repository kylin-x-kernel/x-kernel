# tipc_handle — Security and Reliability Analysis

## Trust Model

This analysis covers the entire hand-written crate: [lib.rs](../src/lib.rs),
[handle.rs](../src/handle.rs), [handle_set.rs](../src/handle_set.rs), and
[handle_table.rs](../src/handle_table.rs). Dependency internals and macro-generated
code are outside this inventory; their soundness remains a dependency assumption.

```text
User IPC arguments -> tipc syscall adapter -> process-local HandleTable / HandleSet
Trusted IPC objects <--- Handle callbacks --- table and set operations
                         |
                   kpoll / kspin / alloc
```

The protected assets are `HandleTable.handles` (process-local object ownership),
`HandleSetEntry.handle` and `handle_id` (event-to-object association), and waiter
registrations (progress and lifetime). `Arc` ownership must remain valid; IDs
must be resolved in the intended process table and paired with the right object.

The crate trusts kernel `Handle` implementations to report their kind faithfully,
expose their concrete value through `as_any`, synchronize state, and honor poll,
registration, and idempotent close contracts. It does not isolate a malicious
kernel implementation. Process selection, object authorization, and user-memory
validation belong to `kprocess` and the `tipc` adapter, not to this crate.

## External Boundaries / Attack Surface

| Boundary | Input and direction | Validation and responsibility |
|---|---|---|
| B-01: process-local IDs | User-derived `i32` values pass through the adapter into table lookup/removal | `uctx_handle_get` and uninstall reject absent IDs with `KError::BadFileDescriptor`; the adapter's `get_handle` selects the current process table |
| B-02: set registration | Adapter passes `HandleSetCommand` and `HandleSetEntry` into `handle_set_ctrl`; polling returns `UEvent` | Local checks reject duplicates, missing entries, cookie mismatches, and nested sets. Callers supply a consistent ID/object pair; cookies are opaque values, not credentials |
| B-03: kernel object callbacks | Table/set code calls methods on supplied `Arc<dyn Handle>` objects and receives events/registration results | Trusted implementations own object state, callback behavior, and close effects; set polling holds its lock across child callbacks |
| B-04: wait and cleanup integration | Callers supply `PollContext`, retain registration owners, and invoke table cleanup | `kpoll` owns registration storage; callers recheck readiness, release wait owners, and close handles at lifecycle boundaries |

The [syscall adapter](../../tipc/src/syscall.rs) reads `UserEvent` through
`UserPtr::read_vm`, rejects unknown commands in `handle_set_command`, and rejects
unknown event bits with `HandleEventMask::from_bits`, both as `KError::InvalidInput`.
It resolves the set and child through current-process lookups before calling this
crate. These lookups do not make the whole registration operation atomic against
concurrent close; kernel callers likewise must coordinate the association.

This crate has no direct user-pointer, MMIO/PIO, DMA, FFI, or assembly interface.
It does not parse firmware, boot metadata, device input, file contents, or network
packets. User-derived IPC IDs, masks, and cookies remain relevant through the
adapter above. IRQ interaction is through `kspin` guards and `kpoll` wake-ups,
not a hardware input parser.

## Unsafe Code Inventory

The four hand-written source files contain no `unsafe` blocks, functions, traits,
implementations, FFI declarations, or assembly. `extern crate alloc` imports a
Rust crate; it is not an FFI boundary. Unsafe behavior inside dependencies is
outside this inventory, so absence of local unsafe is not a whole-kernel proof.

## Memory-Safety Invariants

Tables, entries, and snapshots retain `Arc<dyn Handle>` references; checked `Any`
type tests/downcasts identify sets without raw-pointer casts. Object allocation
can outlive table removal or explicit close. These Rust ownership guarantees do
not establish logical openness, process authorization, or freshness of an ID.
There are no additional local unsafe preconditions to inventory.

## Thread Safety

| State | Protection and remaining obligation |
|---|---|
| Table mappings, set index, snapshot cache | Mutations require `&mut HandleTable`; shared process access uses an external `ksync::RwLock`. Callers coordinate multi-step operations |
| Set entries | `SpinNoIrq` serializes changes and polling; child `kind`/`poll` must not block or re-enter the set |
| Wait registrations | `PollSet` synchronizes storage; callers retain `PollRegistrations` across pending polls and clear/drop them on completion or error |
| Object cookie | `AtomicUsize` uses release stores and acquire loads; this does not synchronize all object state |

`Handle: Any + Send + Sync` constrains stored objects; this crate has no manual
`Send`/`Sync` implementation. A set snapshots children before registering them
outside its lock. `PollSet::wake` invokes wakers outside its source lock, but
external caller locks can still be held. Allocation, IRQ/preemption guards, and
callback requirements are detailed in [design.md](design.md).

## Threat Analysis

Severity: **High** means UB, memory corruption, or privilege escalation;
**Medium** means panic, unavailability, or inconsistent state; **Low** means
performance degradation or reduced functionality.

| ID | Threat | Severity | Trigger | Response and residual risk |
|---|---|---|---|---|
| T-01 | Access to another process's handle object (B-01) | High | A caller selects another process's table or treats an ID as globally authoritative | The adapter selects `current_user_process`; table lookup rejects absent IDs. Correct process selection remains an external obligation; the table has no credential check |
| T-02 | Stale or mismatched registration reports the wrong logical handle (B-01/B-02) | Medium | An ID is reused, a registration races with close, or a caller pairs an ID with a different object | Uninstall detaches the ID from sets still installed in the table before ordinary remove closes it. `Arc` preserves allocation, not identity freshness. Cross-operation races and retained/uninstalled sets are not covered; stronger caller serialization or generation-tagged IDs would be additional hardening |
| T-03 | Old IPC capabilities survive an exec/exit boundary (B-04) | High | Integration drops references without explicitly closing objects | With TIPC enabled, `Process::apply_exec_update` closes the table on the successful exec-update path; `do_exit` calls `Process::close_all_tipc_handles` before exit publication. Object close behavior and successful runtime access remain external dependencies; table Drop alone is insufficient |
| T-04 | Recursive set polling or reference cycles prevent progress/release (B-02/B-03) | Medium | Add/modify supplies a set as a child | `handle_set_ctrl` rejects `HandleKind::HandleSet` with `KError::InvalidInput`; the contract depends on truthful kernel implementations. Non-cyclic nesting is also unsupported |
| T-05 | Handle and registration growth exhausts memory or CPU time (B-01/B-02/B-04) | Medium | A caller repeatedly installs objects, grows sets, or registers waits | Full ID-space scans return `TooManyOpenFiles`; fallible registration/snapshot allocation reports `NoMemory`. There is no local resource quota, and ordinary collection allocations are infallible APIs. Quotas would be additional hardening; allocator failure policy remains external |
| T-06 | Callback re-entry deadlocks a set or caller lock (B-03/B-04) | Medium | Child polling blocks/re-enters under `SpinNoIrq`, or a waker reacquires a caller-held lock | The `Handle` contract forbids blocking/re-entrant set polling; child registration and source wake callbacks run outside their respective internal locks. Arbitrary implementation callbacks and external lock ordering remain caller obligations |
| T-07 | A waiter misses progress or retains stale registrations (B-04) | Medium | It omits table-change registration/rechecks, drops its owner before pending, or retains partial registrations after failure | Cache invalidation notifies table waiters; the adapter registers objects and table changes before rechecking. `PollRegistrations` clear/Drop cancels queued waits. Other callers must follow the same protocol; notifications do not store readiness |

## Failure Modes and Effects Analysis (FMEA)

Severity: **1** fatal, **2** serious, **3** moderate, **4** minor.

| ID | Failure mode | Cause | Local effect | System effect | Severity | Response |
|---|---|---|---|---|---|---|
| F-01 | Lookup/removal rejected | Absent or negative ID | `BadFileDescriptor` | One IPC operation fails | 3 | Table lookup/uninstall rejects the ID without closing another entry |
| F-02 | Registration command rejected | Duplicate/missing ID, cookie mismatch, or nested set | `AlreadyExists`, `NotFound`, or `InvalidInput` | Requested set update is not applied | 3 | `handle_set_ctrl` preserves entries on error; caller reconciles its state |
| F-03 | Empty set cannot yield an event | No registrations | `poll_one` returns `NotFound` | Wait must be rebuilt or abandoned | 4 | Distinguish empty-set error from `Ok(None)` on a nonempty set |
| F-04 | Wait registration fails partway | Storage allocation, exhausted registration IDs, or child rejection | `PollRegisterError`; earlier registrations may remain | Wait cannot proceed as requested | 3 | Caller clears/drops the owner; adapter maps errors through `map_register_error` |
| F-05 | New handle cannot be installed | Every nonnegative ID is occupied | `TooManyOpenFiles` | Process cannot obtain another ID | 2 | Return exhaustion; release existing IDs before retrying |
| F-06 | Ordinary allocation fails | Table/set growth or snapshot construction exhausts the allocator | No local recoverable allocation result | May terminate kernel execution depending on allocator policy | 1 | No local recovery guarantee; resource limits would require additional policy |
| F-07 | Explicit cleanup is omitted or bypassed | Only table Drop runs, or runtime cleanup fails | Objects can remain logically active | IPC ownership survives its intended lifetime | 2 | Exec/exit integration invokes close-all; inspect integration errors and concrete close contracts rather than relying on reference release |

## Failure Handling

Table/set operations use `KResult`; waiter registration uses
`Result<(), PollRegisterError>`. In the adapter, `map_register_error` maps
`NoMemory` and `IdExhausted` to `KError::NoMemory`, and `InvalidState` to
`KError::InvalidInput`. Neither successful registration nor a wake proves
readiness, so callers must recheck before waiting again.

Single-entry remove detaches registrations in installed sets before close.
Close-all first empties the table and invalidates its cache, then closes each
object; it does not detach every registration before those callbacks.
`HandleSet::close` clears its entries without closing children. Close callbacks
return no error status. The crate has no local retry, panic recovery, or quota
mechanism; it cannot turn arbitrary callback/allocator failures into `KResult`.

Lifecycle integration is in [Process runtime access](../../../process/kprocess/src/process/runtime_access.rs)
(`apply_exec_update`, `close_all_tipc_handles`) and the
[process exit path](../../../posix/process/src/runtime.rs) (`do_exit`).

## Privacy Analysis

The crate retains IDs, event masks, cookies, and references to IPC objects, but
does not inspect message payloads or memory-reference contents. Cookies can
carry application meaning and are returned as event metadata. There is no local
logging, redaction, encryption, or zeroization policy. Disclosure control depends
on correct table selection and caller handling; retained snapshots can extend
object lifetime after table removal.

## Known Limitations

- IDs have no generation tag; object references may remain valid after close.
- Set ID/object association and multi-step atomicity are caller responsibilities.
- Nested sets and set transfer are unsupported; ascending-ID polling is not fair
  and finalizes child events before filtering them.
- No local quota bounds collections or ID scans. `Arc` retention can delay release.
- No blanket IRQ/early-boot safety guarantee applies to allocating/callback paths.

## Audit Checklist

- [ ] Resolve user-derived IDs in the intended table; preserve ID/object association across concurrent operations (T-01/T-02).
- [ ] Preserve single-remove detachment, close-all ordering, and explicit exec/exit close calls (T-02/T-03).
- [ ] Keep truthful `kind`/`as_any`, nonblocking poll, and idempotent close contracts for new `Handle` implementations (T-04/T-06).
- [ ] Preserve fallible registration error handling and registration-owner cleanup; assess new growth paths and quotas (T-05/T-07).
- [ ] Register table changes and object events before rechecking readiness; retain the owner while pending (T-07).
- [ ] Update the unsafe inventory if a hand-written unsafe, FFI, or assembly boundary is introduced.

Existing checks in [tipc tests](../../tipc/src/tests.rs) include
`handle_set_matches_trusty_empty_and_nested_rules` (T-04/F-03),
`handle_table_close_detaches_handle_set_entries` and
`handle_table_forgets_removed_handle_sets` (T-02), and
`handle_set_is_not_sendable` (transfer restriction).
[Process tests](../../../process/kprocess/src/tests.rs) include
`test_tipc_handles_close_before_process_exit_publication` (T-03 cleanup ordering).
These cover the named conditions, not every race or failure above. For remaining
controls, review the referenced lookup, command, wait, and cleanup branches
against the trigger and response columns. Referencing these tests does not
assert that they were executed for this documentation change.

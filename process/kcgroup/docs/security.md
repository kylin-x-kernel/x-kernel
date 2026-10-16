# kcgroup security and reliability

## Scope, trust model, and input boundaries

The whole crate (`src/lib.rs` and `ut-tests/lib.rs`) is covered. Assets are canonical membership,
stable task identities, pids quotas/counts, topology, and live-node reservations.
Trusted kernel adapters supply child names, limit values, nodes, and task handles.
The cgroup2fs/syscall layer must enforce mount visibility, credentials,
permissions, and target-task access before mutations. Process-wide migration
requires the additional `kprocess` cgroup gate and publication protocol.

`create_child` rejects empty, dot, dot-dot, and slash-containing names, but is not
full pathname/credential validation. Numeric pids limits are bounded by
`PIDS_MAX_LIMIT`; hierarchy queries compare live object identity. No direct user
pointer, MMIO, DMA, firmware, file-content read, FFI, or assembly exists here.
The crate denies unsafe code at its root.

## Invariants and thread safety

Hierarchy transaction locking serializes multi-node edits. Per-node locks
protect maps/controller references, atomics protect count/limit snapshots, and
membership locks protect attachment. A pending charge keeps its node reserved;
commit transfers accounting to a task-owned membership exactly once. Stable
`TaskIdentityToken` ownership preserves identity for the entire membership entry
across an `exec` leader takeover.

Removal may complete only with no children, direct members, or reservations.
Group migration validates all source hierarchies before altering counts.
The core trusts upper-layer process gates for whole-thread-group consistency;
it does not make publication and migration across process state atomic itself.
Operation guards keep a node live, but do not freeze quota/delegation policy.

## Threat analysis

| ID | Threat and asset | Severity | Trigger | Response and residual risk |
|---|---|---|---|---|
| T-01 | Unauthorized membership/quota change | High | Untrusted request reaches trusted mutation APIs | cgroup2fs/syscall adapters own authorization; core checks topology/accounting only. A raw node handle is not permission. |
| T-02 | New task bypasses pids quota | Medium | Fork publishes without an admission charge | `reserve_task` checks active controller limits and rolls partial counts back; callers must reserve before publication and commit exactly once. |
| T-03 | Detached node accepts a task | Medium | Delayed task construction races removal | Lifecycle reservations pin the node; removal returns `EBUSY` while charges/operation guards exist. Inactive targets reject new admission/migration. |
| T-04 | Mixed-hierarchy migration partially changes counts | Medium | Membership list includes another hierarchy | `migrate_group` checks all sources before mutations and returns `EXDEV`; overflow additions are rolled back. |
| T-05 | Numeric reuse overwrites membership | High | Different tasks share a numeric projection | Membership is a set of stable `TaskIdentityToken`s, so distinct tasks never collide; duplicate commit returns `EEXIST`. Readers resolve the token to the live task and read the current pid. |
| T-06 | Stale filesystem handle mutates removed node | Medium | File operation starts after removal | `begin_operation` returns `ENODEV`; adapters must acquire/retain its guard for their complete operation. |
| T-07 | Limits collide with unlimited sentinel | Medium | Numeric input exceeds the supported PID domain | `set_pids_max` returns `EINVAL` above `PIDS_MAX_LIMIT`; only `None` selects unlimited. |
| T-08 | Thread group split by migration/publication race | High | Prepared sibling publishes after group target changes | `kprocess` process gate and publication reconciliation are required in addition to the hierarchy transaction. |
| T-09 | Counter/depth resource exhaustion | Medium | Extreme reservations, namespace IDs, or hierarchy nesting | Controller count additions/subtractions are checked; reservation/ID increments and hierarchy depth remain unbounded locally. Trusted adapters must constrain workloads. |

## Failure modes and effects (FMEA)

| ID | Failure mode | Cause | Local effect | System effect | Severity (1-4) | Handling |
|---|---|---|---|---|---|---|
| F-01 | Admission fails | Active pids limit or count overflow | `EAGAIN`, partial charge released | Fork/clone rejected | 3 | RAII rollback of `TaskCharge`. |
| F-02 | Child mutation fails | Invalid name, missing node, duplicate name, busy node | `EINVAL`/`ENOENT`/`EEXIST`/`EBUSY` | Filesystem operation fails | 3 | Preserve original child and restore ACTIVE on busy removal. |
| F-03 | Controller inaccessible | Node removed or controller absent/inactive | `ENODEV` or `ENOENT` | Controller operation rejected | 3 | Adapter reports error; no fabricated active state. |
| F-04 | Charge invariant fails | Incomplete commit, inactive commit, count underflow | Assertion or debug assertion | Kernel service may fail | 2 | Preserve exact reservation/accounting ownership; checked subtraction does not wrap. |
| F-05 | Hierarchy mismatch | Unrelated source/target or view root | `EXDEV` | No successful migration/path result | 3 | Validate before mutation or rendering. |
| F-06 | Allocation exhaustion | Map/vector/node creation | Allocation cannot complete | Service unavailable | 2 | Global allocator policy; no local recoverable allocation path. |

## Failure handling and privacy

Failures use `KError` converted from explicit Linux errors. Ordinary rollback is
RAII-based; there is no retry loop around admission. Detach is idempotent and
returns no error. `commit` assertions and decrement debug assertions expose
internal invariant violations, not recoverable user-input validation.

Paths, namespace IDs, and membership snapshots reveal process relationships;
this crate does not log or copy user payload. Visibility checks belong to
adapters. The system initial namespace is retained globally and must remain the
same hierarchy used by boot filesystem setup.

## Known limitations and audit checklist

- Existing-task migration intentionally ignores `pids.max`.
- Retain the hierarchy root while using weak-parent ancestry results.
- Preserve gate -> publication -> hierarchy lock ordering in integration.
- Keep error paths from dropping/replacing preexisting membership identities.
- Do not treat live-operation guards or path rendering as authorization.
- Recheck wrap/depth limits if exposing broader untrusted creation workloads.

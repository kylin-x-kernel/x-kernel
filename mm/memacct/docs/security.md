# memacct — Security Document

## External Boundary / Attack Surface

- Physical page addresses supplied by allocator and page-cache callers.
- Allocation classes supplied by kernel callers.
- The current-memcg provider function pointer installed by the process layer.
- Cgroup ids read from page metadata during release.
- Byte counts and limit values originating from cgroup control files.

No user pointer is dereferenced by this crate.

## Trust Boundaries

`charge_frame` trusts its caller to own the physical range for the duration of
the call. `uncharge_frame` trusts the caller to release exactly the range that
was charged. The page allocator and `PageRef` reference protocol enforce that
ownership; `memacct` does not grant page access.

The provider is trusted kernel code. It must not sleep or re-enter the
accounting lock. The function pointer is installed once and never replaced.

## Memory Safety Invariants

- A page's memcg id is written only after its entire range has resident page
  metadata and the hierarchical charge succeeded.
- A failed charge leaves no page id and no counter delta.
- A page id is cleared exactly once by `uncharge_frame`.
- A dead node is not removed while local or subtree usage is nonzero.
- Ids are 32-bit and never reused, so a stale page id cannot alias a new node.
- `Memcg` lookup returns an `Arc`; callers cannot use a freed node.

## Unsafe Code Inventory

- Function-pointer publication/read for the current-memcg provider.
  `install_current_memcg_provider` publishes only a valid function pointer;
  `current_memcg` transmutes it back after an Acquire load. The pointer is never
  replaced.
- No raw page or user-memory access is performed by `memacct`.

## Failure Modes and Effects Analysis

| Failure | Handling | Effect |
|---|---|---|
| Page metadata missing | Reject before charging | No partial charge, allocation caller frees the page |
| Charge exceeds `memory.max` | Roll back ancestor counters | `NoMemory`, `max` event once |
| Task migrates after fault | Page keeps recorded id; uncharge uses it | Charge follows physical page |
| Task exits before uncharge | Id lookup is task-independent | No leak or wrong-owner release |
| Cgroup removed while pages charged | Mark dead, retain until last uncharge | No id reuse/alias |
| Duplicate uncharge | Page id already zero | No counter underflow |
| Provider not installed | Return `NONE` | Early-boot pages uncharged |
| Provider re-enters accounting | Provider contract forbids it | Documented execution-context rule |

## Execution Context Contract

- `current_memcg`: safe in ordinary and IRQ context; must not sleep.
- `charge_frame` / `uncharge_frame`: allocator/IRQ-safe, no sleeping.
- `remove`: cgroup teardown context; takes `CHARGE_LOCK`, no allocation.
- `Memcg` counters: lock-free reads; writes serialized by `CHARGE_LOCK`.

## Known Limitations

- No reclaim implementation, so `low`/`high` events remain zero.
- No OOM killer, so `oom`/`oom_kill` remain zero.
- No swap backend.
- Page-meta coverage for hot-added memory is the hotplug layer's responsibility.

# memacct — Design Document

## Positioning

`memacct` owns the memory-cgroup accounting tree and the page-attribution
metadata contract. It does not implement reclaim, OOM killing, memory hotplug,
or the cgroup filesystem; those policies belong to `kcgroup`, `cgroup2fs`, and
the memory hotplug layer.

The crate has two responsibilities:

1. maintain hierarchical byte counters, limits, peaks, and page-class stats for
   each `Memcg` node;
2. keep a physical page's original accounting owner valid until the last
   physical reference is released.

## Identity and Lifecycle

`MemcgId` is a monotonically allocated 32-bit identity. Zero is `NONE`.
Identities are never reused, including after a node is reclaimed.

A `Memcg` node holds:

- an optional parent link used for hierarchical charges and events;
- a `dead` flag set when its cgroup controller is detached;
- local and subtree usage/peak counters;
- limit values;
- page-attribution counters.

`remove(id)` marks a node dead. The node remains in the lookup table while it
has a live local or subtree charge, because page metadata may still refer to
the id. The final `uncharge` removes it from the table. A dead node with no
charges cannot be observed by new callers.

## Charging

Charges are byte-based and hierarchical. A charge walks from the target node to
the root, adding local and subtree usage. A limit failure rolls back every
counter already touched by that charge and leaves no partial hierarchy.

`CHARGE_LOCK` serializes charge/uncharge commits so limits, peaks, and rollback
are coherent. The lock is IRQ-safe and short-lived; it never calls allocators,
reclaim, OOM handlers, or page-table code.

Ordinary counters use `AcqRel` for add/sub because the commit lock orders
concurrent updates and the values are used by readers that must observe a
complete commit. Limit loads use `Acquire` and limit stores use `Release` so a
new limit is observed with the state that justified it. Event and peak updates
are independent monotonic counters; their `Relaxed`/`AcqRel` usage is local to
the counter and never publishes another field.

## Page Attribution

`charge_frame(paddr, bytes, kind)` queries the installed current-memcg provider
and charges the allocation. The function requires resident page metadata for
every page before it changes any counter. It then records the memcg id and the
allocation class in each page descriptor.

`uncharge_frame(paddr, bytes)` reads the recorded owner and class from the
descriptors, clears the owner, and releases the charge. It does not consult the
current task, so a page can be released after the allocating task migrated or
exited.

The page ownership is tied to the physical allocation, not to a cache object's
destructor. `PageRef` calls `charge_frame` when it admits a range and calls
`uncharge_frame` from its final `release_unit` path. `Folio` therefore does not
charge or uncharge by itself; a page table or another `PageRef` holder can keep
the physical page alive after the cache identity is gone.

## Execution Context

The current-memcg provider is a function pointer installed by the process layer.
It may inspect the current task's cgroup membership. It must not sleep and must
be safe in ordinary and IRQ context. Before installation the provider returns
`NONE`, so early boot allocations are not charged.

`charge_frame`/`uncharge_frame` require initialized page metadata and may be
called from allocator/IRQ-safe context. `Memcg::remove` may be called from the
cgroup controller teardown path; it takes the same accounting lock as a charge.

## Limits, Events, and Peaks

`max` is enforced at charge time. A rejected charge increments `max` once on
the node whose limit was hit and propagates `NoMemory`.

`low` and `high` are retained as thresholds for a future reclaim implementation.
This kernel has no reclaim path, so charging does not fabricate `low`/`high`
events. `oom` and `oom_kill` remain zero because no OOM killer is invoked by
this crate.

Peaks are monotonic high-water marks and can be reset under `CHARGE_LOCK`.
Stats distinguish anonymous and file-backed charges using the allocation class
recorded by the caller; the class is not inferred from a stale page descriptor.

## Failure Handling

- Missing page metadata: return `NoMemory` without charging.
- Limit rejection: roll back the whole ancestor chain and return `NoMemory`.
- Removed node with live pages: retain it until the last uncharge.
- Unknown/dead id on uncharge: ignore rather than underflow another cgroup.

## Non-goals

- Reclaim, throttling, and OOM policy.
- Memory hot-add/remove and page-meta table extension.
- cgroup filesystem rendering and delegation.
- Swap accounting; this kernel has no swap backend.

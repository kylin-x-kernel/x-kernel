# kidentity design

## Purpose and background

`kidentity` owns the process domain's PID/TID number-space model. `kns` re-exports
its namespace type; task/process creation and cgroup membership consume stable
`Arc<PidHandle>` identities. Lookup tables, process publication, scheduling,
permissions, and complete PID-namespace syscall semantics belong to upper
layers. Keeping number allocation separate lets those owners share identity
without introducing a catch-all process crate dependency.

## Scope and architecture

`src/lib.rs` contains all types, allocation functions, and kernel unit tests.

```text
PidNamespace { id: u64, parent: Option<Arc<_>>, level: u32, next_nr: AtomicU32 }
       ^
       | Arc ownership
PidHandle { numbers: Vec<Upid>, lifecycle: AtomicU8 }
  -> Upid { nr: u32, ns: Arc<PidNamespace> }
```

The namespace parent chain is immutable. Each root/child allocator starts at
one. `root_pid_namespace` lazily publishes a shared root with `LazyInit`.
`PidHandle` stores number projections from the active namespace to the root and
a [`PidLifecycleState`] that gates observer visibility.

`PidNamespace::id()` is a kernel-wide monotonic identity stable for the
namespace's lifetime; procfs uses it as the `/proc/<pid>/ns/pid` inode so
`stat()` can distinguish PID namespaces.

## Execution context

The crate has no current-task, scheduler, address-space, or CPU-local dependency.
Allocation requires a working heap. It does not intentionally sleep, although
lazy initialization may wait for another initializer and allocation follows the
allocator's context contract. Pure accessors are usable wherever a valid handle
can be borrowed; do not assume allocating APIs are interrupt-safe merely because
the number counter is atomic. Early process initialization can use them once
allocation is available.

## Allocation and lookup algorithms

`allocate_in` first collects the ancestor chain, then increments each namespace's
counter with checked `fetch_update` (AcqRel success, Acquire failure). Exhaustion
returns `WouldBlock` without wrapping. Already consumed numbers in descendant
namespaces are not rolled back if a later ancestor allocation fails. There is no
reuse on drop; `u32::MAX` itself is not returned because incrementing it fails.

`root_nr` finds the root projection and expects one to exist. `nr_in` first
compares namespace `Arc` identity; a missing non-root namespace yields `None`.
A different root namespace deliberately falls back to the root projection.
`fixed_root(nr)` creates a root-only projection without reserving or checking the
counter, so trusted callers must prevent conflicting numeric registrations.

### Observer projection

`number_in(observer)` returns the number held in exactly that namespace.
`visible_number(observer)` is the projection an observer resolves: starting at
the observer's namespace it climbs to the root and returns the number at the
deepest common ancestor that holds a projection for the identity. An observer
inside the identity's namespace hierarchy always sees a number (the root
projection is the terminal fallback); an observer rooted in an unrelated
namespace tree receives `None`. This mirrors Linux `struct pid`/`upid` lookup
semantics without storing a raw PID integer in observers.

## Lifecycle state machine

Every [`PidHandle`] carries a [`PidLifecycleState`]:

```text
Reserved --(publish: mark_published)--> Published --(retire: mark_retiring)--> Retiring
Retiring --(reap: mark_reclaimed)--> Reclaimed
Reserved --(abort: mark_reclaimed)--> Reclaimed
Published --(reap with no waiters: mark_reclaimed)--> Reclaimed
```

- `Reserved`: number allocated, task not yet published — not lookup-visible.
- `Published`: task live, number authoritative — lookup-visible.
- `Retiring`: exit in progress; waiters may still resolve the identity —
  lookup-visible (Linux `pid_allocated` keeps exiting tasks findable).
- `Reclaimed`: reaped — no longer resolvable.

Transitions are closed under `PidLifecycleState::try_transition` and enforced
with a compare-exchange (`AcqRel`), so publication and reclamation order the
matching writes. The dedicated marker methods return `false` on illegal
transitions instead of panicking. Task owners (`kprocess` publication/exit) are
the integration points that drive these transitions; this crate defines and
tests the state machine so upper layers cannot invent invalid transitions.

## Concurrency and design decisions

`next_nr` and the lifecycle byte are the only mutable atomics. Immutable handles
and ancestor references can be shared without locks. A vector and linear lookup
keep the representation simple for shallow namespace trees; no depth cap or
number reclamation is implemented. Stable allocation is distinct from
publication: callers must finish identity registration before making tasks
runnable.

The first ordinary root allocation is expected to serve PID 1 during boot.
Boot/idle/ordinary kernel workers use PID-less identities in their owners; this
crate does not reserve special roles or cache an init-only handle.

## Drop and limitations

Dropping a handle frees its vector and namespace references once the final `Arc`
is gone. Children retain their ancestors, but ancestors do not retain children,
so this graph has no ownership cycle. Counters never decrement. Namespace depth
addition is not checked, and fixed projections need external uniqueness policy.
This module alone does not provide complete Linux PID namespace isolation or
drive the lifecycle transitions from task publication/exit; those belong to the
process layer.

# tipc_handle — Design Document

## Role

`tipc_handle` owns process-local handle IDs, shared object references, and event
aggregation for X-Kernel. `process/kprocess` stores its `HandleTable`, while
`tee/tipc` implements concrete objects through `Handle` and uses `HandleSet`
for event selection.

`tipc` owns message transport and syscall argument/user-memory handling;
`kprocess` selects the process and synchronizes its table. This crate does not
schedule waits. API contracts and examples belong in rustdoc; threat analysis
belongs in [security.md](security.md).

## Background

Trusty IPC exposes integer handles for ports, channels, sets, and memory
references. The process runtime needs to own these objects across syscalls and
release them at exec or exit. Keeping the table in `tipc` would make `kprocess`
depend on the IPC core, whose syscall adapter already uses `kprocess`.
This crate separates their shared ownership and event contracts from concrete
IPC behavior to avoid that dependency cycle.

## Scope

The complete crate consists of [Cargo.toml](../Cargo.toml) and four source files:

| File | Responsibility |
|---|---|
| [src/lib.rs](../src/lib.rs) | Private module declarations and public re-exports |
| [src/handle.rs](../src/handle.rs) | `Handle`, event bits, object kinds, notification and cookie storage |
| [src/handle_set.rs](../src/handle_set.rs) | Registration commands, `HandleSetEntry`, `UEvent`, and `HandleSet` |
| [src/handle_table.rs](../src/handle_table.rs) | ID allocation, ownership, removal, and wait snapshots |

## Architecture

```text
kprocess::ProcessRuntime -- owns RwLock<HandleTable> --> tipc_handle
 tipc syscall adapter   -- table and set operations --> tipc_handle
 tipc_handle            -- Handle callbacks --------> tipc objects
 tipc objects          -- HandleWaitState ----------> kpoll
```

| Interface group | Provider and consumer |
|---|---|
| `HandleTable` lookup, install, remove, snapshots | Process runtime and syscall adapter call this crate; `ProcessRuntime` owns the table under its `tipc` feature |
| `HandleSet` creation, control, polling | Syscall adapter supplies resolved objects and receives `UEvent` values |
| `Handle` kind, poll, register, close, cookie, downcast, sendability | This crate defines the contract; `tipc` supplies `IpcPort`, `IpcChan`, and `MemRef`, and this crate supplies `HandleSet`; table/set code and the adapter call these objects |
| `HandleWaitState` registration, notification, cookie | Concrete objects, sets, and the table use this crate's wrapper over `PollSet` and `AtomicUsize` |

`HandleTable.handles` maps IDs to `Arc<dyn Handle>`; `handle_set_ids` indexes
installed sets for removal cleanup. Each `HandleSetEntry` also owns an `Arc`.
A registration's cookie is independent of the object's cookie. Callers must
associate an entry's ID and object with the same process-local table; the set
does not resolve that association itself.

The crate is `no_std` with `alloc` and has no Cargo features or conditional
source modules. Its dependencies are `bitflags` for event masks, `kerrno` for
errors, `kpoll` for wait registrations, `kspin` for synchronization, and
`smallvec` for the set-ID index (`const_new` and `union` features).

## Calling Constraints / Execution Context

The crate does not require a current process thread; callers supply objects,
tables, and `PollContext`. The syscall adapter supplies task context and
scheduler-backed waiting. Registration itself records wakers rather than
sleeping. Do not hold a table lock across a blocking wait or a pending poll.

Construction and collection growth require an initialized allocator and mapped
kernel memory. Spin-lock operations require architecture IRQ support and any
enabled preemption hooks, which operate on the current CPU. There is no direct
MMIO or address-space manipulation in this crate. Early-boot use is valid only
once the allocation and guard facilities needed by that path are available.

`HandleWaitState::notify` is IRQ-safe through `PollSet::wake`; this does not make
allocating table/set operations generally IRQ-safe. Child `kind` and `poll`
callbacks execute under the set's IRQ-disabling lock and must neither block nor
re-enter the set. Table mutations and close can invoke callbacks under
caller-held locks; callers must exclude lock-order cycles and re-entry into
those locks.

## State Machines

State is represented by collection membership and cache presence, not a
lifecycle enum. `HandleKind` classifies objects and `HandleSetCommand` selects
operations. The following transitions describe those actual data changes:

| Owned state | Trigger | Result |
|---|---|---|
| Unoccupied table ID | Successful `uctx_handle_install` | ID owns a strong object reference |
| Occupied table ID | `uctx_handle_uninstall` / `uctx_handle_remove` | Mapping is removed; only `remove` also closes the object |
| Missing set entry | Successful `Add` | Entry stores the object, event mask, and cookie |
| Existing set entry | Successful modify / delete command | Entry is replaced / removed; failed checks leave it unchanged |
| Populated set | `HandleSet::close` | Entries are cleared; subsequent additions remain valid |
| Missing snapshot cache | `wait_any_snapshot` | Cache is populated from current table membership |
| Cached snapshot | Successful install/uninstall or `uctx_handle_close_all` | Internal cache is cleared; externally held snapshots remain unchanged |

Object-specific open/closed states belong to the `Handle` implementation in
`tipc`. Removing a table reference and closing the object are separate actions.

## Algorithms and Flows

### Install and remove

`uctx_handle_install` scans free nonnegative IDs from `next_id`, wrapping after
`i32::MAX`. It inserts the object, indexes concrete sets using checked `Any`
downcasts, then invalidates the wait cache and notifies table waiters.
ID-space exhaustion terminates the scan with an error.

`uctx_handle_uninstall` removes the mapping, removes its set index if applicable,
detaches the ID from sets still installed in this table, and invalidates/notifies
the wait cache. It returns the object without closing it, supporting speculative
install rollback. `uctx_handle_remove` follows this with `Handle::close`.

### Set registration and event selection

`handle_set_ctrl` adds, replaces, or deletes entries under the set lock, then
notifies set waiters after a successful operation. Add rejects duplicates;
modify/delete require an existing ID. Cookie-checked commands additionally
require equality with the stored cookie. Add/modify reject objects reporting
`HandleKind::HandleSet`, preventing nested polling and reference cycles.

`poll_one` visits IDs in ascending order and calls each visited child's
`poll(true)` before applying its event mask. It returns the first match with
the registration cookie. An empty set is distinguished from a nonempty set
without matching events. Filtering can discard events already consumed by a child;
selection does not rotate for fairness. In contrast, the set's `Handle::poll`
uses child `poll(false)` and reports aggregate `READY` without consuming events.

### Waiting

`wait_any_snapshot` caches an ID-ordered `Arc<[(i32, Arc<dyn Handle>)]>` so repeated
polls do not clone the whole table. Membership changes invalidate the cache,
but previously returned snapshots retain their objects and may become stale.

A set's `Handle::register` registers its own notification source, snapshots child
objects and masks under the set lock, then registers children outside that lock.
Registration errors propagate to the caller; partial registrations remain
owned by its `PollRegistrations`.

The [syscall wait paths](../../tipc/src/syscall.rs) retain `PollRegistrations`
across pending polls. Wait-any registers both snapshot objects and table
membership changes, then fetches the current snapshot and rechecks readiness.
This covers a handle becoming ready or being installed during registration.
Notifications are one-shot hints; callers register again and recheck after waking.

## Concurrency Model

| Shared state | Synchronization |
|---|---|
| `HandleTable` fields | Caller-owned synchronization; `ProcessRuntime` uses `ksync::RwLock` |
| `HandleSet.entries` | `SpinNoIrq<BTreeMap<i32, HandleSetEntry>>`; local IRQs and, when enabled in `kspin`, preemption are disabled |
| Waiter storage | `kpoll::PollSet` protects registrations internally; wakers run outside its source lock |
| `HandleWaitState.cookie` | Release stores and acquire loads on `AtomicUsize` |

The set lock spans child polling, but child waiter registration uses a snapshot
and runs after releasing it. This limits callback nesting during registration;
polling still requires the nonblocking, non-reentrant contract above. The table's
wait cache avoids allocation and per-object reference cloning on unchanged
wait-any rounds, at the cost of keeping old objects alive in retained snapshots.
Cookie ordering does not replace synchronization of the object's other state.

## Design Decisions

- **Shared ownership crate:** separating the table from `tipc` avoids a
  `kprocess`/`tipc` dependency cycle while leaving IPC behavior with its owner.
- **Sets colocated with the table:** removal needs to detach registrations in
  installed sets, so both types share the same ownership boundary.
- **Trait objects rather than a concrete-object enum:** `Arc<dyn Handle>` lets
  `tipc` provide object types without becoming a dependency of this crate.
- **No nested sets:** rejecting set children avoids recursive polling and set
  reference cycles; non-cyclic nesting is also unsupported.
- **Ordered maps and cached snapshots:** IDs provide deterministic traversal,
  while snapshot reuse reduces repeated wait-any work. Selection does not
  rotate for fairness, and ID allocation can scan the full occupied space.

## Drop / Resource Release

`uctx_handle_close_all` first empties the table, clears the set index, resets ID
allocation, and invalidates/notifies the wait cache. It then calls `close` on
each removed object; it does not detach every registration before those calls.
`HandleSet::close` clears its entries and wakes waiters without closing children.
Dropping a table only releases references; explicit close is needed for IPC
shutdown effects. Old snapshots and external `Arc` references can outlive close.
Dropping the last `PollSet` source wakes remaining waiters; dropping registration
owners cancels still-queued registrations.

With TIPC enabled, [Process::apply_exec_update](../../../process/kprocess/src/process/runtime_access.rs)
closes the table on its successful exec-update path. The last-thread exit path
in [do_exit](../../../posix/process/src/runtime.rs) calls
`Process::close_all_tipc_handles` before publishing process exit. Concrete close
behavior belongs to `tipc`: ports are unpublished and channel peers are notified.

IDs can be reused and multiple IDs can refer to one object; a strong reference
preserves allocation, not logical openness. Cleanup only tracks sets still
installed in the same table. Set transfer is unsupported; external ID/object
validation and synchronization remain caller responsibilities.

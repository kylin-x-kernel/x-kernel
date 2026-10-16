# kcgroup design

Kernel unit tests live in `ut-tests/` and are mounted from their original
source modules with `unittest::path_mod_test!` under `cfg(unittest)`.

## Purpose and scope

`kcgroup` owns the canonical cgroup v2 hierarchy, pids controller accounting,
and task membership. The implementation is in `src/lib.rs`; its kernel tests
are in `ut-tests/lib.rs`.
`kprocess` integrates task publication, process-wide migration, and exit;
cgroup2fs and syscall adapters provide authorization, ABI parsing, and views.
They must not maintain parallel membership or controller state. This crate
provides no filesystem implementation or credential-based authorization.

## Background and architecture

A fork needs a reversible admission charge before a visible task exists, while
migration of an existing task must transfer accounting without applying the
new-task limit. `TaskCharge` and `TaskMembership` separate those lifetimes.

```text
CgroupNamespace -> Arc<Cgroup> view root
Cgroup -> strong children / weak parent / shared CgroupHierarchy
       -> optional PidsController
       -> member_tasks: set of stable TaskIdentityToken
reserve_task -> TaskCharge -> commit -> TaskMembership -> detach/drop
```

`CgroupNamespace::initial` lazily publishes the system hierarchy once, available
before PID 1 so boot cgroup2fs setup and init share it. `new` creates an independent
hierarchy for isolated callers/tests; `new_view` creates a view of an existing
node without migrating tasks. `CgroupNamespaceId` is a separate relaxed atomic
ID allocator, not an authorization token.

## Execution context and synchronization

Hierarchy mutations, reservations, migration, and detach use a hierarchy-wide
sleepable `Mutex<()>`. Children and optional controller references use `RwLock`;
membership maps use `Mutex`; each task's current cgroup uses `RwLock<Option<_>>`.
Atomic controller values permit cheap snapshot reads. Lifecycle, reservations,
and delegation bits are atomic; multi-node edits occur under the transaction.
`set_pids_max` updates its atomic limit after controller validation without
acquiring the hierarchy transaction itself.

Use task context with allocation and sleepable locking available, not IRQ or
atomic/spinlocked context. There is no local current-task or CPU dependency;
initial namespace construction is valid during boot once these services exist.
Do not reenter the hierarchy transaction while holding it. `kprocess` adds an
outer process cgroup gate so migration and publication agree on a thread-group
target; integration uses process gate -> publication lookup -> hierarchy
transaction ordering. The core does not acquire that process gate for callers.

## Controller and task flows

The hierarchy root has no pids controller files. Enabling `pids` in a parent's
subtree activates controllers on direct children. A non-root node must receive
the controller from its parent and have no direct members before delegating;
a parent cannot disable it while a child still delegates. Inactive controllers
remain accounting anchors; reactivation resets the limit to unlimited.
The first activation initializes a child's count from its subtree membership.
Numeric limits must be at most `4 * 1024 * 1024`; `None` represents textual `max`
using a private `usize::MAX` sentinel. Lowering a limit below current usage is
allowed and affects later admission.

`reserve_task` increments a lifecycle reservation and charges each controller
in root-to-leaf order. A failure drops the partial charge and releases counts.
`TaskCharge::commit` checks for a duplicate stable `TaskIdentityToken` before
publishing it in the membership set. A duplicate returns `EEXIST` and rolls
back the new charge. The token is allocated once per task and never reused, so
it stays valid across an `exec` leader takeover, and readers resolve the live
task from it to read the current pid.

`migrate_group` sorts/deduplicates memberships by stable identity token, locks
their current nodes, validates every source hierarchy, and charges target-only
controller suffixes before changing membership. It skips detached memberships
and unchanged targets. Mixed hierarchies return `EXDEV` before counts change;
count overflow rolls back added counts. Migration ignores pids limits but
rejects a non-root target that delegates controllers (`EBUSY`).

## Node lifecycle and path queries

Non-root lifecycle is `ACTIVE -> REMOVING -> REMOVED` during child removal.
The remover validates exact child identity, reserves removal, and checks direct
members, children, and outstanding reservations. A busy check restores ACTIVE;
success unlinks the node and deactivates its controller. Operation guards and
pending task charges prevent removal from completing while they are live.
New filesystem operations on an inactive node receive `ENODEV`.

`is_descendant_of` and `common_ancestor` traverse under the transaction lock.
`path_from` compares live lineage roots and rejects unrelated roots; nodes
outside a view render `..` components, which describe position rather than grant
access. Parent links are weak: callers need the canonical hierarchy owner alive
when interpreting ancestry/path snapshots.

## Decisions and resource release

Strong child links and weak parents avoid ownership cycles. Membership retains
stable task handles rather than only reusable numeric projections. An uncommitted
charge drops its reservation and charged controllers in reverse order.
Membership detach/drop removes its index entry and releases controller lineage
counts; repeated detach is harmless. `CgroupOperationGuard::drop` releases its
reservation. Checked decrement prevents wrap; underflow triggers a debug
assertion and otherwise leaves the counter unchanged.

There are no memory/CPU controllers, authorization engine, or full namespace
clone syscall implementation here. Namespace IDs and reservation increments
have no explicit wrap protection. Recursive subtree counts and linear lineage
walks are not depth-bounded by this crate.

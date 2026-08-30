# kcgroup design

`kcgroup` owns the canonical cgroup v2 hierarchy, task membership, and controller
state. Linux ABI and cgroup2fs code are adapters and must not keep a second
membership table or derive controller policy independently.

`CgroupNamespace::initial()` lazily owns the system's initial namespace and
hierarchy. It is available before PID 1 exists, so boot-time filesystem setup and
the init process share one canonical root. `CgroupNamespace::new()` remains the
constructor for an independent hierarchy used by isolated callers and tests.

Each task owns one `TaskMembership`. Fork and clone first obtain a `TaskCharge`;
dropping an uncommitted charge rolls the count back. Publication commits it to a
stable `Arc<PidHandle>` identity and returns `EEXIST` rather than overwriting an
existing identity. The membership map retains that strong identity for its full
lifetime, so the PID allocator cannot reuse the numeric projection while a cgroup
entry still exists. Existing-task migration transfers both the identity and charge
without checking `pids.max`, matching cgroup v2 semantics.

`PidsController` is separate from hierarchy identity. The hierarchy root has no
pids controller files. Enabling `pids` in a node's `subtree_control` activates a
controller state on each direct child; task admission charges the attached
controller lineage and skips the hierarchy root. Inactive controller objects
remain as internal accounting anchors so disable/re-enable preserves an exact
count while files and limit enforcement are absent. Reactivation resets the
limit to `max`.

Numeric `pids.max` values are limited to Linux's PID domain
(`4 * 1024 * 1024`). Unlimited state remains an internal `usize::MAX` sentinel
and is exposed only through the textual value `max`, so numeric input cannot
collide with it.

Controller enable follows cgroup v2 top-down and no-internal-process rules. A
non-root node can delegate `pids` only when it receives the controller from its
parent and has no directly attached tasks. A parent cannot disable `pids` while
a child still delegates it. Existing-task migration ignores `pids.max`, but it
cannot place tasks into a non-root domain with active subtree controllers.

The hierarchy uses strong child links and weak parent links. One hierarchy-wide
sleepable transaction lock serializes topology mutation, controller transitions,
reservation, migration, and detach. Per-controller atomics retain cheap snapshot
reads and checked count updates, but multi-node changes are committed under the
transaction lock.

`Cgroup::path_from()` first proves that the node and namespace view root share
the same hierarchy root. Cross-hierarchy inputs return `EXDEV`; callers cannot
accidentally render an unrelated hierarchy as a relative path.

Each non-root node has an `ACTIVE -> REMOVING -> REMOVED` lifecycle. Removal
first closes admission, then checks descendants and hierarchical task charges.
Reservation and migration revalidate lifecycle after incrementing and roll back
if removal won the race, preventing detached nodes from accepting new members.
Filesystem operations acquire `CgroupOperationGuard` under the same hierarchy
transaction. The guard contributes a lifecycle reservation, so removal cannot
complete during an operation; operations started on a removed node return
`ENODEV`. Removal also deactivates that node's controller view.

`is_descendant_of()` and `common_ancestor()` provide stable hierarchy queries
under the transaction lock. Filesystem adapters can enforce mount-view and
delegation policy without copying parent links or traversing them outside the
canonical synchronization boundary.

`kprocess` adds a process-level cgroup gate above this hierarchy transaction.
Fork/clone selects membership under that gate, whole-process migration updates
all published threads under it, and publication reconciles a prepared sibling
to the process target before making the task visible. The lock order is process
cgroup gate, publication lookup, then hierarchy transaction.

# kcgroup security

The core validates node names and enforces pids admission transactionally. It does
not make authorization decisions: the cgroup2fs adapter must validate mount
namespace visibility, credentials, file permissions, and target task access
before calling mutation APIs.

`TaskCharge` is the rollback boundary for task creation. Callers must keep the
charge unpublished until all fallible construction succeeds, then commit it
exactly once to the task-owned membership. `commit()` checks for a duplicate
stable identity before inserting; `EEXIST` drops the charge and reservation
without replacing the original membership. A dropped membership removes its
reverse-index entry and releases its pids charge.

Membership stores a strong `Arc<PidHandle>`, not only a numeric TID. This pins
the allocator identity until detach/drop and lets adapters distinguish the old
task from a later registry occupant with the same numeric projection.

Counter release uses an atomic checked subtraction, so an invariant violation
cannot wrap `pids.current` to `usize::MAX`. Detached membership queries return
`None`; observers such as procfs treat that as an exited or unavailable task.

Controller availability is top-down. The root cannot expose `pids.max` or
`pids.current`, and a non-root domain with directly attached tasks cannot enable
a subtree controller. Controller transition, group migration, reservation,
detach, and cgroup removal share the hierarchy transaction lock, preventing an
empty-check/removal race or migration into an orphan node.

Whole-process migration additionally requires the `kprocess` process cgroup
gate. Task publication takes the same gate and reconciles prepared siblings to
the committed process target, so fork/clone cannot leave a live thread group
split across domains.

Task reservations pin cgroup lifecycle: a node with an outstanding fork/clone
reservation cannot transition to `REMOVED`. The reservation is released only
when the charge is committed or rolled back, so delayed publication cannot
attach a task to an unlinked cgroup. Group migration validates every source
hierarchy before changing controller counts, so mixed-hierarchy requests fail
atomically with `EXDEV`.

Filesystem operation guards use the same removal reservation protocol. This
prevents an old file descriptor from racing removal, while a descriptor used
after removal receives `ENODEV` instead of mutating a detached node. Stable
subtree and common-ancestor queries run under the hierarchy transaction so an
adapter can bind authorization to the source/destination snapshot used by the
migration transaction.

Numeric pids limits above `4 * 1024 * 1024` return `EINVAL`. This keeps external
values within Linux's PID domain and prevents collision with the private
`usize::MAX` unlimited sentinel.

Namespace-relative path rendering performs the same hierarchy-root validation
and returns `EXDEV` before constructing a path for unrelated roots.

The initial namespace is published once through `klazy::Once`. Boot-time
cgroup2fs construction and the init process both clone that `Arc`; neither path
may create a replacement hierarchy after the other has become visible.

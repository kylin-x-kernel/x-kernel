# Safety and Ownership

Integration task entry preparation uses KERNEL outside read scopes and writer
locks. Allocation failure fails the fixture before its new task is activated;
prepared tasks release captures through the existing task lifecycle. This does
not make stacks, Arc owners or nested fixture allocations recoverable under OOM.

## Entry allocation boundary

Entry preparation precedes publication and writer locking. A rejected allocation
cannot change list topology; dropping an unpublished entry releases its payload
once. Flag-rejection tests check this ownership boundary without forcing global
OOM. Existing host, delayed-reader and callback tests now exercise KBox nodes.
The callback's nonblocking destructor requirement is unchanged and independent
of the flags used to allocate the node.


## Trust Boundary

This crate accepts kernel-owned Rust values, not user pointers, firmware tables
or MMIO. Consumers validate any externally sourced handler identity or payload.
The unsafe mutation contracts are a trusted kernel boundary, not runtime sandboxing.
The crate does not log payloads or provide memory wiping guarantees.

## Publication

Only consumed Entry allocations are published. Writer exclusion protects head,
tail and predecessor updates; links remain atomic for lockless readers. T must
be Send + Sync + 'static. Those bounds do not authorize destructor context or
unsynchronized mutation of T.

## Readers

Iterators require a genuine krcu scope and borrow the list for their full lifetime.
Acquire loads establish initialization. Removed next pointers remain unchanged
until reclamation. Readers cannot escape the scope, mutate or republish nodes.
Untrusted payloads must be validated by consumers before handler invocation.

## Reclamation

`Entry::get_mut` accesses only uniquely owned unpublished payload storage.
Publication consumes Entry, preventing mutable access after readers can see it.

Every linked allocation has one owning list. Asynchronous unlink transfers its
sole retirement right to its embedded callback. Field projection and offset_of preserve allocation
provenance without assuming a particular Rust field order. No exclusive Node
reference is created while readers can observe it. Callback destruction is shallow:
it never follows next, which may already refer to reclaimed storage.

Submission rejection retains storage. Cancellation is not a GP and must not free
the enclosing node. This leak is only an invariant-failure containment measure,
not the ordinary error policy. Admission failure before mutation leaves the list
unchanged. No raw detached handle is exposed to safe callers.

Prepared task removal in `src/take.rs` instead retains the unique raw allocation
in an opaque `RetiredListEntry`; its embedded callback is never submitted.
`withdraw_first` has the same task-origin and nonblocking predicate obligations
as the existing writer path. It may run under a consumer short lock only after
wait preparation. `reclaim` is unsafe because the preparing task must restore
watched sleepable execution with no reader/progress locks before waiting.
The prepared GP is submitted after unlink; only successful completion permits
KBox recovery and transfer of T. There is no callback access to T and no unsafe
claim that Send/Sync makes its destructor nonblocking. Final T destruction stays
with its task owner. Independently owned operations still require consumer drain.

Both handles are task-bound and cannot escape the prepared wait's stack scope.
An unmatched withdrawal has no raw owner and skips waiting. Dropping a matched
retirement without reclamation intentionally leaks its node; the cancelled wait
does not authorize freeing it. Forgetting the handle also leaks its wait marker.
No detached link is reset, reused or traversed by reclamation. The enclosing
prepared-take call retains its list borrow through the body, even though the
retirement handle itself only owns detached storage and its prepared wait.

## Context

Unsafe writer contracts exclude interrupt-origin mutation and reentrancy.
Asynchronous removal additionally requires watched sleepable task context and a
nonblocking, nonunwinding complete destructor chain. Prepared removal instead
returns the payload to its task owner after reclamation; that owner must choose
a context suitable for its destructor. Predicate execution occurs under writer
exclusion. No allocation, scheduling or execution-context transition is permitted
there. Existing IRQ/NMI read admission belongs to krcu, not this container.

Exclusive list destruction is synchronous in the dropping context; callers must
use a context suitable for T just as for ordinary owned Rust containers. It does
not flush callbacks. Consumer teardown must separately ensure code and resources
referenced by deferred callbacks outlive their execution.

## Unsafe Inventory and Failure Modes

| Path | Enforced invariant | Remaining caller obligation |
| --- | --- | --- |
| `src/lib.rs`: push_back/withdraw | Same-list writer exclusion, atomic links, owned allocation | Task-origin mutation; nonblocking predicate |
| `src/lib.rs`: Iter::next | Scope and list borrows, Acquire traversal | Obey krcu scope execution rules |
| `src/lib.rs`: Drop | Exclusive list ownership; no submitted live node | Valid context for the entire payload release chain |
| `src/node.rs`: retire/release | One embedded submission right, offset-based KBox recovery | Nonblocking callback destruction; permanent service |
| `src/take.rs`: withdraw_first/reclaim | Unique opaque retirement, stack/task binding, post-unlink GP | Task-origin predicate; restore sleepable watched context without reader/progress locks before reclaim |

| Failure | Effect | Containment |
| --- | --- | --- |
| Invalid removal context or unavailable service | Operation rejected | No predicate call or unlink |
| Unexpected submission rejection after unlink | Invariant panic | Retain allocation rather than free before GP |
| Long reader or callback backlog | Delayed release and memory growth | Existing RCU progress; no arbitrary container backlog limit |
| Blocking/reentrant predicate or destructor | Deadlock or execution-context violation | Explicit unsafe contract; not dynamically proven by Send/Sync |

Audit link preservation, one-shot ownership, release ordering, and callback
independence whenever these paths change. This container provides neither resource
quotas nor handler execution cancellation, reverse traversal or CPU hotplug policy.

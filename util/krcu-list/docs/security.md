# Safety and Ownership

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

Every linked allocation has one owning list; unlink transfers its sole retirement
right to its embedded callback. Field projection and offset_of preserve allocation
provenance without assuming a particular Rust field order. No exclusive Node
reference is created while readers can observe it. Callback destruction is shallow:
it never follows next, which may already refer to reclaimed storage.

Submission rejection retains storage. Cancellation is not a GP and must not free
the enclosing node. This leak is only an invariant-failure containment measure,
not the ordinary error policy. Admission failure before mutation leaves the list
unchanged. No raw detached handle is exposed to safe callers.

## Context

Unsafe writer contracts exclude interrupt-origin mutation and reentrancy.
Removal additionally requires watched sleepable task context and a nonblocking,
nonunwinding complete destructor chain. Predicate execution occurs under writer
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
| `src/node.rs`: retire/release | One embedded submission right, offset-based Box recovery | Nonblocking callback destruction; permanent service |

| Failure | Effect | Containment |
| --- | --- | --- |
| Invalid removal context or unavailable service | Operation rejected | No predicate call or unlink |
| Unexpected submission rejection after unlink | Invariant panic | Retain allocation rather than free before GP |
| Long reader or callback backlog | Delayed release and memory growth | Existing RCU progress; no arbitrary container backlog limit |
| Blocking/reentrant predicate or destructor | Deadlock or execution-context violation | Explicit unsafe contract; not dynamically proven by Send/Sync |

Audit link preservation, one-shot ownership, release ordering, and callback
independence whenever these paths change. This container provides neither resource
quotas nor handler execution cancellation, reverse traversal or CPU hotplug policy.

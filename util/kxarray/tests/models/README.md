# Publication Memory Models

This standalone host workspace keeps Loom out of the kernel dependency graph
and default unit tests. From the repository root:

```sh
RUSTFLAGS='' cargo +1.95.0 test --locked \
  --manifest-path util/kxarray/tests/models/Cargo.toml \
  --target x86_64-unknown-linux-gnu --release -- --nocapture
```

Do not set `LOOM_*` exploration limits when collecting acceptance evidence.
The lockfile pins the host tool dependencies. A prior kernel build may leave
`cfg(unittest)` rustflags in the workspace configuration; clearing RUSTFLAGS
prevents those flags from being inherited by these host-only tests.

## Mapping To Production

| Model | Implementation boundary | Forbidden result |
| --- | --- | --- |
| Root publication | `PreparedNodes::take`, `XArray::publish` root store, `lookup_slot` root load | Visible new root with uninitialized height or contents |
| Child then payload | `Node::insert_slot` publishes an empty child before installing the already initialized payload; `lookup_slot` acquires each link | Visible child with uninitialized height |
| Existing branch | A branch retained by another entry receives a newly initialized owner in an empty slot | Visible payload with uninitialized value |
| Writer handoff and growth | Writer exclusion transfers the old tree to another writer, which publishes a new root retaining that tree | Visible new root with uninitialized metadata or lost old subtree contents |

Each model uses identities instead of raw pointers. Relaxed atomic data fields
represent initialization writes, so negative controls can expose stale data
without invoking a Rust data race. Arc retains all simulated storage until the
threads join. The writer Mutex models only the exclusion and memory ordering
of `writers`, not its IRQ/preemption guards. The reader does not take that lock.
The child model allows absence before payload installation; it does not demand
a whole-tree snapshot or that all readers immediately see a concurrent insert.

Four positive tests retain the production Release/Acquire publication chain.
Five negative controls weaken one root store, root load, child store, payload
store or grown-root store to Relaxed. They must panic with the matching invariant
message; ordinary test success includes finding these intended counterexamples.
The separate existing-branch case avoids wrongly attributing payload visibility
to its own slot Release when a newly created child's Release already publishes
that initialization. The growth case checks the combined ordering chain; it
does not isolate the necessity of the writer lock's ordering from the other
Acquire/Release edges.

## Evidence Boundary

These are small ordering models, not the production radix implementation.
Changes to publication sites must be checked against this mapping; changing the
implementation alone will not automatically change a model. The normal host,
Miri and kernel tests exercise actual code and remain necessary.

The models assume retained storage, initialized runtime and correctly serialized
writers. They do not establish RCU grace-period detection, callback eligibility,
pruning/reclamation safety, pointer provenance, interrupt admission, hardware
NMI delivery, allocation failure behavior or scheduler progress.

Loom explores a subset of weak-memory executions and explicitly does not model
the full C11 memory model; in particular some load-buffering executions are
missing. Passing is supporting evidence for these finite scenarios, not a
complete Rust memory-model proof. See the project's
[documented limitations](https://github.com/tokio-rs/loom#unsupported-features).
These models use Acquire/Release/Relaxed and do not depend on Loom's SeqCst
access approximation.

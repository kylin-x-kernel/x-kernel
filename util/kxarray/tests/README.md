# Host Storage Checks

Standalone, manually run weak-memory publication models and their explicit
assumptions are documented in [models/README.md](models/README.md). They do not
add a kernel dependency or run as part of the default unit-test suite.

Run from the workspace root:

```sh
RUSTFLAGS='' cargo +1.95.0 test -p kxarray --target x86_64-unknown-linux-gnu
RUSTFLAGS='' cargo +nightly-2026-03-08 miri test -p kxarray --lib --target x86_64-unknown-linux-gnu
```

Clear inherited kernel rustflags: a previous `make unittest` can leave
`cfg(unittest)` in the generated Cargo configuration. Host tests use `cfg(test)`
and must not link the kernel's registered test bodies or platform providers.

The host modules test the production insertion, lookup, withdrawal and destruction paths,
not a copied radix implementation. The dev dependency enables `kspin/smp` so
host threads receive real writer exclusion; local IRQ guards use kspin's host
no-op implementation. Primitive payloads have no kernel destructor dependency.

These tests do not manufacture `ReadScope`. Private lookup pointers are used
only while the fixture retains all traversed storage. Miri checks
the executed ownership, alignment and aliasing paths, including an existing
payload borrow across root growth. It does not validate kernel admission,
interrupt exclusion, grace periods or every possible concurrent interleaving.
Pruning fixtures use the real locked withdrawal path, retain old raw nodes and
payloads across reinsertion, then explicitly end their borrows before invoking
the actual shallow node callbacks. They check detached path lengths and empty
roots; Miri also checks allocation release. This does not simulate GP detection
or concurrent destructive updates without a real read scope.

The public API doctests check both scope and container lifetime constraints,
with a compile-success example for independently acquired Arc ownership.
Kernel tests in `../ut-tests/` exercise real scoped reads, task-backed competition,
interrupt access and migration. They run after boot initializes RCU; no XArray
fixture runs in the startup path. Run them with `make unittest UNITTEST_CRATE=kxarray`.
Kernel-only dependencies support these registered tests because this runner builds
normal libraries with `cfg(unittest)`, not Cargo test targets. Test bodies are
absent from ordinary builds; host storage tests do not compile kernel dependencies.

Retirement fixtures invoke the actual callback and cancellation functions on
private records. They check that cancellation preserves a still-borrowed raw
payload, GP-eligible release consumes one owner, and an external Arc survives.
Borrowers are ended explicitly before release; this simulates callback
eligibility, not GP detection. Kernel tests separately hold two actual read
scopes across successive replacements and drain callbacks after reader exit.
The removal fixture holds an actual old-value scope while another CPU removes
the deepest path and reinserts the same key. It checks the absent intermediate
state, new lookup, old-borrow validity and exact destruction after reader exit.
Spinning readers and their coordinator have disjoint CPU affinities, restored
before reporting results. A separate multi-task removal fixture requires exactly
one successful withdrawal and all other callers to report absence. That test
does not force every possible lock-preparation interleaving.

Startup-before-RCU behavior is not covered by this ordinary kernel suite.
Callback-before-submit-return safety is checked by auditing that successful
ownership transfer is followed by no record access; these tests do not force
that timing in the live service.

`../ut-tests/context.rs` uses actual timer hardirq delivery to
read successive replacement, absence and reinsertion states while retaining a
sibling entry. Each one-shot invocation acquire-loads a stage request published
after mutation and verifies exactly that stage's value. The callback publishes
completion or an invalid-read flag; the task waits for that acknowledgement and
synchronously cancels the invocation before the next mutation. Earlier
observations cannot satisfy later stages. The two-millisecond timer delay
requests delivery, not a maximum accepted latency. Observation waits have no
guest-clock deadline; run with an external QEMU timeout as described below.
Missing delivery terminates that disposable run without in-guest cleanup.
Cancellation/draining retain normal service-progress dependencies.
This tests ordered IRQ reads, not overlap with publication.
Another fixture reads through nested scopes in the live RCU callback executor
and verifies serving-softirq context before scope entry. Callback records own
their container shares and release them before completion reporting. These
fixtures do not establish NMI or interrupted-writer-window coverage.

The controlled NMI fixture holds the actual array writer lock, enters the
kernel's balanced software NMI test path and performs nested interrupt reads.
It checks existing, replaced, absent and reinserted values, retained siblings,
zero successful allocation bytes on the pinned CPU and restoration of reader/enrollment/cleanup,
watching and IRQ state. The lock helper exists only in kernel unittest builds;
production mutation paths gain no callback or branch. This fixes exclusion at
a stable tree state, not an intermediate publication instruction. It is not
evidence of hardware NMI delivery or early exception-entry support. A lock-taking
reader regression may hang this fixed window; the outer QEMU timeout remains
the deadlock detector, since a timeout on the same locked stack cannot recover.
Allocation accounting does not detect unsuccessful allocation attempts; the
read implementation is also inspected for absence of allocation calls.

`../ut-tests/migration.rs` requires an SMP runtime with at least three CPUs. The
coordinator stays on CPU0 while a reader keeps its old-value borrow across an
affinity-enforced CPU1-to-CPU2 migration. The reader confirms its destination
from its resumed stack; a successful affinity API result alone is insufficient.
The fixture checks original-node enrollment, old-value retention through scope
exit, eventual unlinking and exact owner destruction. Release/acquire handshakes
wait for the reader's own destination-CPU acknowledgement without guest-clock
latency thresholds. Cleanup signals exit, joins the reader and restores
coordinator affinity before reporting failed operations. Run these kernel tests
with an external QEMU timeout (for example `timeout --kill-after=10 600 make
unittest UNITTEST_CRATE=kxarray`); a stalled scheduler fails that run rather than
being judged against an internal five-second performance requirement. Task join,
affinity enforcement and callback draining retain their normal kernel-progress
dependencies. This does not exercise writer migration between commit and submit.

The prepared-writer fixture reuses the one-shot preparation checkpoint after
replacement preflight and retirement-record allocation, before locked
reinspection. CPU0 asks the scheduler to move the paused writer from CPU1 to
CPU2; the writer confirms its destination without voluntarily yielding. The
checkpoint retains no read scope or writer lock. Before releasing the writer,
the coordinator verifies that the old association and owner remain intact.
After completion it verifies the new association and drains callbacks to check
exact destruction of both versions. This validates migration between preflight
and mutation/submission, not specifically between the swap and submission.
The checkpoint and installer are kernel-unittest-only; ordinary builds retain
the original mutation path. The same external timeout and cleanup limits as the reader
migration fixture apply.

The partial-publication fixture installs a separate one-shot checkpoint just
after a new child link is stored, before any payload is installed in that child.
It runs balanced software NMI reads while the actual writer lock is still held.
Growing from key 0 to `usize::MAX` first verifies lookup through the old root
while the new root is private. Adding key 64 afterwards verifies lookup through
an already published root into a newly linked empty leaf. Both observations
must retain existing values and return absence for the not-yet-installed key;
after insertion returns, that key must be present. Per-stage counts prevent a
missing checkpoint from passing using an earlier observation. The observer
only reads and updates atomics in a retained anchor payload, without allocating
or waiting. Hook storage, installation and invocation are all `cfg(unittest)`.
This adds a fixed intermediate publication window, not hardware NMI delivery or
a weak-memory proof. The outer QEMU timeout detects same-stack lock deadlocks.

# Host Storage Checks

Unique owners and private nodes/retirement records use KBox. Fixtures use explicit
NOWAIT permissions unless they specifically exercise default sleepable task APIs.
Host storage tests inject per-call reservation failure along partial paths and
check that no root or payload is published. The failure loop covers all path
reservations, including both branches needed to grow an anchored root. A host-only
System allocator counts successful allocations and deallocations on the current
test thread around the operation, asserting immediate release before array teardown;
other threads and fixture setup are excluded. Miri also checks the released private
storage. Kernel fixtures additionally reject retirement allocation for set/remove
before mutation and verify returned-owner identity, conflict precedence and
invalid flags. Injection affects only the private preparation interface, not
global OOM, nested payload allocation or kernel reclaim behavior.

Standalone, manually run weak-memory publication models and their explicit
assumptions are documented in [models/README.md](models/README.md). They do not
add a kernel dependency or run as part of the default unit-test suite.

Prepared insertion tests allocate a full private capacity token, change root
shape before commit, and count zero allocations during publication. Conflict
returns the unchanged owner and deallocates all unused private nodes. Failure
injection covers each node preparation allocation; cancellation also balances
all private allocations without an RCU provider. A compile-fail doctest rejects
token reuse. The kernel fixture verifies prepared publication through real RCU
reads. This is not coverage of a paired IRQ-index transaction.

Prepared removal tests cover allocation failure, invalid flags and private
cancellation without an RCU provider. The kernel test prepares a record, masks
IRQs, and verifies rejection before withdrawal; a new record then removes the
entry in admitted context and real callback draining releases exactly the array
owner. Missing-key removal and compile-time single-use are covered too. These
checks do not prove paired-index atomicity or every interleaving of readers and
destructive updates.

Split-phase tests withdraw two private indexes under a caller-held SpinNoIrq
lock, then submit their pending retirements after unlocking. They verify that
both array owners are retained until submission and released after callback
drain. The optional SMP integration fixture holds one real read scope across
both withdrawals and submissions, confirms identical borrowed payload identity,
then checks exactly one final destructor after reader exit. Compile-fail doctests
reject transferring or reusing a pending retirement. These are container protocol
tests, not proof that KIRQ's paired-index publication has been integrated.

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

Locked ownership queries use the public `get_owned_locked` entrance without
an RCU provider. Host checks cover missing/boundary keys, pointer identity,
strong counts, no allocation during lookup, concurrent root growth and an owner
surviving array destruction. The kernel removal fixture retains both a scoped
and a locked-query Arc across actual callback draining. Neither test establishes
early-boot CPU-slot initialization or permits locked queries from IRQ/NMI/BH
handlers; those continue to use scoped RCU reads.
Pruning fixtures use the real locked withdrawal path, retain old raw nodes and
payloads across reinsertion, then explicitly end their borrows before invoking
the actual shallow node callbacks. They check detached path lengths and empty
roots; Miri also checks allocation release. This does not simulate GP detection
or concurrent destructive updates without a real read scope.

The public API doctests check both scope and container lifetime constraints,
with a compile-success example for independently acquired Arc ownership.
Basic kernel tests remain in `../ut-tests/`. The [kernel integration crate](../crates/kernel-tests/README.md)
owns task-backed competition, interrupt access and migration. Both run after boot
initializes RCU, without XArray-specific startup fixtures. Run the complete set with:

```sh
timeout --kill-after=10 600 make unittest UNITTEST_CRATE=kxarray,kxarray_kernel_tests
```

Filters use Rust module names: `kxarray_kernel_tests` has underscores, unlike its
Cargo package name `kxarray-kernel-tests`. Host storage tests do not depend on
the integration crate or its task/platform providers.

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

`../crates/kernel-tests/ut-tests/context.rs` uses actual timer hardirq delivery to
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

`../crates/kernel-tests/ut-tests/migration.rs` requires an SMP runtime with at least three CPUs. The
coordinator stays on CPU0 while a reader keeps its old-value borrow across an
affinity-enforced CPU1-to-CPU2 migration. The reader confirms its destination
from its resumed stack; a successful affinity API result alone is insufficient.
The fixture checks original-node enrollment, old-value retention through scope
exit, eventual unlinking and exact owner destruction. Release/acquire handshakes
wait for the reader's own destination-CPU acknowledgement without guest-clock
latency thresholds. Cleanup signals exit, joins the reader and restores
coordinator affinity before reporting failed operations. Run these kernel tests
with an external QEMU timeout (for example `timeout --kill-after=10 600 make
unittest UNITTEST_CRATE=kxarray,kxarray_kernel_tests`); a stalled scheduler fails that run rather than
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

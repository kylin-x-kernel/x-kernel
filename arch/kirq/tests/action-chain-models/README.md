<!-- SPDX-License-Identifier: Apache-2.0 -->
<!-- Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/> -->
<!-- See LICENSES for license details. -->

# Action-chain ordering models

Standalone host workspace; Loom is not a kernel dependency. This follows
`util/kxarray/tests/models`, not a replacement ActionSet implementation.
These models cover ordinary running ownership only;
PerCpu reclamation now requires caller-completed source shutdown and is not modeled.

Run outside the kernel workspace, with no `LOOM_*` exploration limits:

```sh
cd /tmp
fixture=/root/codes/x-kernel-worktrees/kirq-action-lifecycle/arch/kirq/tests/action-chain-models/Cargo.toml
RUSTFLAGS='' CARGO_TARGET_DIR=/tmp/kirq06-action-model-target \
  cargo +1.95.0 test --locked --offline --manifest-path "$fixture" \
  --target x86_64-unknown-linux-gnu
RUSTFLAGS='' CARGO_TARGET_DIR=/tmp/kirq06-action-model-target \
  cargo +1.95.0 clippy --locked --offline --manifest-path "$fixture" \
  --target x86_64-unknown-linux-gnu --tests -- -D warnings
```

The committed lockfile pins host dependencies. No kernel configuration or build
is involved. Each model has at most two participating threads, fixed node
identities, and bounded operations except the predicate/Condvar wait. There is
no polling loop or timeout. No permutation, duration or preemption truncation
is configured by the fixture. Loom's default branch safety bound still applies;
exceeding it fails a test rather than establishing successful exploration.
Negative tests stop at their first named counterexample, as intended.

## Correspondence

All production paths below are relative to `arch/kirq/src/runtime/`.

| Model | Production operations | Checked obligation |
| --- | --- | --- |
| `append` | `action_set.rs`: `ActionSet::publish` initializes sequence/payload before Release tail-link store; `IrqActionSnapshot::with_actions` acquires successor then reads sequence before ceiling rejection | A cursor reaching appended C through existing B sees initialized metadata; an old ceiling excludes C |
| `cached_successor` | `with_actions` caches next before callback; `ActionSet::withdraw` bypasses B without clearing B.next | A reader already holding B can continue to C; new topology skips B |
| `lifetime` | `dispatch.rs`: running admission and snapshot capture under the same Line lock, delayed unlocked traversal, original completion, then `finish_admission`; `manager.rs`: management-serialized ordinary unlink, `IrqDrain::wait`, `RetiredAction::reclaim` | Reclamation cannot precede old traversal or original completion under ordinary running ownership |
| survivor test | `withdraw_action_on_line` does not gate non-last removal; post-unlink capture uses updated membership under Line | Survivor admission remains possible while management is retained and cannot recapture the removed node |

The append case starts after capture of A and B with ceiling 2. Only B.next and
C's initialization are variable; no reader reacquires the writer lock or waits
for publication. Relaxed atomic metadata stands for ordinary initialization so
broken variants can expose stale values without causing Rust undefined behavior.
The payload read of excluded C is an additional publication oracle; production
only needs to inspect its sequence before rejecting it.

The cached-successor case deliberately forces capture-before-unlink using
channels. This tests preserved topology, not weak-memory publication. Storage
is retained throughout that case. Lifetime is tested separately with an old
captured identity, logical reclaim and two access events (metadata/link, callback)
followed by original completion. There is no address reuse or generation ABA.
The model keeps running set through completion. Production may clear it earlier
inside a continuously held Line-lock tail; drain cannot observe that intermediate
state. The model does not explore that tail's individual controller operations.

The lifetime oracle mutex records access/completion/reclaim events; it is NOT a
production lock and supplies no execution admission. It never spans a callback
or the drain. Reclamation decisions use only Line state. Observations are saved
at the event and asserted after joining workers, avoiding panic-time destruction
of live Loom primitives. Backing Arc storage survives logical reclamation, so
negative variants never dereference freed host memory.

`Condvar` models the locked predicate wait/recheck, not the kernel Completion
algorithm, deferred wake/reset, or scheduler implementation. Management has one
writer; its retained mutex records the assumed serialization interval but does
not establish correctness against a second competing writer. The survivor case
orders a new accepted arrival after the old running owner finishes, while
management remains held. Its Condvar selects that interleaving; production IRQ
dispatch does not block waiting for running ownership. It checks
topology/admission, not reclamation or a production waiter wakeup.

## Negative controls

Each uses `should_panic(expected = ...)` and must find the matching violation:

- Relaxed append store or successor load: `append initialization`.
- Clearing the detached successor: `detached successor preserved`.
- Running admission delayed past head capture, omitted drain, or release before completion:
  `reclamation before original completion`.

These are six single-fault negative controls and four positive scenarios.
An unrelated panic or exploration-limit failure does not satisfy the expected
invariant message.

## Recorded validation

2026-11-06: the revised suite passed all 10 tests (four positive scenarios, six
expected-counterexample controls), with zero failures or ignored tests. Offline
Clippy with `--tests -- -D warnings` also passed. No `LOOM_*` environment variables
were set and no exploration truncation was added. Both commands ran from
`/tmp` with the isolated target directory shown above.

The removed counted-lifetime and overlap scenarios are not retained as coverage
of the source-owner contract.

## Limits

These are finite protocol models, not execution of production Rust. Changes to
the mapped functions require re-review of this correspondence; production edits
do not automatically alter the models. The actual-code kernel tests for captured
head, cached successor, ceiling, survivor dispatch, completion, and destruction
remain necessary. Direct-import host/Miri validation would separately address
raw-pointer/provenance concerns; this fixture makes no such claim.

No proof is provided for unbounded lists/readers, generation reuse, sequence
exhaustion, allocation failure, destructor reentry, directory RCU, IRQ masking,
controller completion internals, PMU/NMI hardware, CPU migration, replay queue
service, hardware quiescence, fairness, or performance. PerCpu execution,
source-owner stop/drain/restart exclusion and unsafe PerCpu free preconditions
are outside this model; no internal PerCpu counter is assumed.
Keeping the successor and lifetime scenarios separate
also means their composition is a reviewed proof obligation, not an explored
whole-chain reclamation algorithm.

Loom explores a subset of weak-memory executions, not the complete C11/Rust
memory model. In particular, its documented limitations include missing some
load-buffering executions. Passing means the modeled finite scenarios completed
without an invariant violation, not exhaustive verification of ActionSet.

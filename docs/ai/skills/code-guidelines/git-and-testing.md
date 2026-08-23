# Git And Testing

Use this file when the task includes test updates,
bug-fix validation, or commit/PR structuring guidance.

## Commits

- one logical change per commit;
- separate refactoring from feature work;
- keep pull requests focused on a single topic;
- write imperative, descriptive commit subjects.

## Cargo.lock

The kernel workspace root `Cargo.lock` and the independent
`xtask/Cargo.lock` are tracked for reproducible builds.
They are machine-generated and must stay consistent with their `Cargo.toml` files;
never hand-edit them — they record exact versions and content checksums,
and a manual merge can leave it inconsistent or unparseable.

The kernel builds for four targets
(`x86_64-unknown-none`, `aarch64-unknown-none-softfloat`,
`riscv64gc-unknown-none-elf`, `loongarch64-unknown-none-softfloat`),
so the committed lockfile must cover all of them, not just the host.

### Resolving `Cargo.lock` conflicts during rebase

A conflict means both branches changed dependency versions.
Because the file is generated, resolve it by regenerating,
not by editing conflict markers by hand:

```bash
rm -f Cargo.lock                                   # drop the conflicted file
cargo generate-lockfile                            # regenerate from the merged Cargo.toml
cargo metadata --locked --format-version=1 >/dev/null   # verify it is a valid locked resolution
git add Cargo.lock
git rebase --continue
```

`cargo generate-lockfile` resolves the graph across all four kernel
targets, so the regenerated lockfile stays complete for every platform.
`cargo fetch` produces the same lockfile but also downloads crate
sources, so prefer `generate-lockfile` at rebase time.
`cargo metadata --locked` must succeed before continuing — it is the
same check `scripts/check_deps.py` runs, and a failure means the
lockfile is out of sync with `Cargo.toml`.

For a single dependency bump rather than a full conflict,
update just that crate and let Cargo reconcile the rest:

```bash
cargo update -p <crate-name>
```

## Tests

- add a regression test for every real bug fix when practical;
- test user-visible behavior rather than implementation details;
- use assertion helpers instead of manual log inspection;
- clean up resources after each test;
- name tests after the behavior or specification concept being verified.

### Unit Test Determinism And Isolation

Unit tests should verify local semantic contracts, not performance,
scheduler timing, or real-device timing. A unit test may check that an
operation becomes observable only when progress is driven by an explicit,
deterministic action such as `run_one_work()`, `run_pending_softirqs()`,
`dispatch_actions()`, `complete()`, `wake()`, or a test-owned provider hook.

Avoid these patterns in ordinary unit tests:

- wall-clock thresholds such as "must finish within N ms";
- median/min/max timing assertions;
- fixed retry loops that assume "N yields is enough";
- reliance on real device IRQs, SMP timing, QEMU speed, or host load;
- reliance on test execution order or global state initialized by earlier tests;
- leaving global hooks, waiters, queues, IRQ mappings, pending bits, worker
  hosts, or other runtime registries installed after the test.

Preferred patterns:

- test pure state machines directly where possible;
- use scoped RAII fixtures to install and restore global state;
- use unique test IDs or allocate conflict-free resources instead of hard-coded
  live IRQ, MSI, or device mappings;
- use explicit synchronization flags, completions, or test hooks to confirm a
  spawned task reached the intended state before asserting;
- use deterministic drivers such as `run_one_work()`, `run_pending_softirqs()`,
  `dispatch_actions()`, or provider test hooks instead of sleeping;
- move timing, throughput, fairness, latency, and stress checks to integration,
  performance, or CI stress harnesses.

For IRQ, softirq, workerqueue, timer, and scheduler tests, any test touching
global runtime state must either use a scoped guard that restores the previous
state on drop, or document why the initialized state is intentionally permanent
for the whole unit-test run.

## When Reviewing

Check specifically for:

- fixes that changed behavior without adding regression coverage;
- tests over-coupled to internal implementation names;
- resource leaks across test cases;
- unit tests that rely on timing, fixed yield counts, or unscoped global state;
- feature work mixed with unrelated cleanup in the same commit series.

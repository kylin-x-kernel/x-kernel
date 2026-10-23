# Completion validation

## Scope

Validation targets the public completion/token and predicate-wait contracts,
their task adapter, and the existing migrated callers. The intrusive list is
the production implementation, not a mock. Host tests replace only IRQ locking
with a mutex; guest tests exercise the actual scheduler and IRQ path.

## Contract coverage

| Contract | Evidence |
| --- | --- |
| Counted tokens cannot be duplicated | Host `competing_consumers_cannot_duplicate_tokens`: eight concurrent consumers compete for 0/1/3/8 tokens; consumed count must equal supplied count. Guest `counted_completion_wakes_one_task_per_token_and_publishes_data` supplies one token at a time to four blocked tasks and checks acknowledgements. |
| Sticky completion and reset | Guest `completion_tokens_sticky_and_reinit`, `saturation_matches_linux_sticky_completion`; host already-ready and broadcast tests. Reset is tested only under its documented lifecycle contract. |
| Early notifications and spurious returns | Host/guest duplicate-registration, FIFO/token-stealing and predicate-reentry tests; task adapter tests cover early tokens, actual blocking and spurious task wakeups. |
| Removal preserves neighbouring nodes | Host `unlink_head_middle_and_tail_preserves_other_waiters` enrolls three distinct stacks in known order, then exits the head/middle/tail normally and by unwinding. The selected stack is joined before broadcasting to the two survivors. Survivors park on latched notification, not timing delays. |
| Concurrent notification cannot outlive stack storage | Host `notification_racing_scope_exit_never_retains_stack_nodes` races notification against successful predicate return for 64 independently scoped lifetimes, checks callback counts and verifies no registration remains after joining. Miri checks invalid stack access. |
| Data published before completion is visible | Host `completion_publishes_payload_without_an_external_release` uses relaxed payload accesses and a completion wait; its setup handshake runs waiter-to-producer, not producer-to-reader. Guest counted-wakeup test also publishes/reads a relaxed payload. The source lock provides the publication edge; these tests supplement, not replace, that ordering argument. |
| Deferred notification retains its source without publishing stale state | Host/guest deferred-owner, dispatch-with-live-owner and reset-before-dispatch tests. |
| Wait/notification do not allocate | Thread-local allocation/free counters include a positive control. Measured cases cover construction, ready paths, actual cross-thread wait/notify, repetition, spurious returns, broadcasts, deferred dispatch with a live owner, and competing token consumers. |
| Context and actual IRQ wakeup | Task adapter rejects atomic/BH-disabled contexts; tracing integration test enables scheduler tracing, wakes a blocked task from timer IRQ, and restores trace configuration. |
| Migrated users retain lifecycle semantics | Kernel suite includes IRQ synchronization, join, kpoll, pidfd, workqueue cancellation/drain, worker detachment, RCU, block, RNG and VirtIO regressions. |

The new task test owns a completion-and-join cleanup guard so an assertion
failure does not strand its worker tasks. Host unwind tests deliberately catch
panics and validate node retirement; they do not model nonunwinding task exit.

## Test sensitivity

Two disposable copies of the host probe were modified, never the worktree:

- Disabling token decrement made the new competing-consumer test fail: eight
  consumers succeeded from one supplied token.
- Omitting registration Drop unlink made the positional-removal test fail under
  Miri on access to a retired stack node.

These negative controls show that the tests detect the targeted broken
contracts. They do not constitute exhaustive mutation testing.
Logs: `/tmp/completion-mutation-{tokens,cleanup}.log`.

## Reproduction

Use the host probe README for the manifest command and stable compiler's
explicit `RUSTC_BOOTSTRAP=1` requirement. Run the same production-core tests
under `cargo +nightly-2026-03-08 miri test`; this validation varies
`MIRIFLAGS=-Zmiri-seed=0` through `3` to sample different schedules.

Prepare `platforms/kplat-aarch64/qemu_defconfig` and run `make defconfig`, then:

```sh
make clippy
make unittest UNITTEST_CRATE=kcompletion,ktask,kpoll,kirq,kworkng,kworkqueue_ng,kworkerpool_ng,krcu,kdriver,kprocess,ktracing,hwrng,virtio,block,device_res_xkernel XKMAKE_ARGS='--no-net --no-vsock'
```

## Recorded results (2026-10-23)

- `make clippy`: passed; existing upstream dependency warnings remain.
- AArch64 QEMU TCG, four vCPUs, independent disk, no network/vsock:
  756 passed, zero failed/ignored; coverage exported successfully.
- Production-core host suite: 17 passed.
- Miri seeds 0, 1, 2 and 3: 17 passed each (68 case executions).
- Both disposable negative controls failed on their intended violations.

Logs: `/tmp/completion-validation-{clippy,ut,host}.log` and
`/tmp/completion-validation-miri-{0,1,2,3}.log`.

## Limits

- Schedule sampling and Miri do not exhaust every possible SMP/IRQ interleaving.
- Host allocation counters do not measure kernel scheduler allocations. Guest
  execution validates integration; the no-allocation scheduler argument also
  depends on code review and the default unfiltered tracing configuration.
- There is no global kernel allocator-failure injection, performance acceptance,
  or validation on other architectures in this run.
- Work/Queue observation receipts, RCU callback markers and lazy BioWait setup
  retain preparation allocations. This is not a claim that whole drain or I/O
  lifecycles are allocation-free.
- Hardware liveness, arbitrary tracing filters, forced nonunwinding task exit,
  unsupported reset races, timeout and interruptible waits are not certified.

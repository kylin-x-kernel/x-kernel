# Completion host probe

Run from outside the kernel workspace to avoid generated target configuration:

```sh
cd /tmp
RUSTC_BOOTSTRAP=1 cargo +1.95.0 test --manifest-path /absolute/path/to/x-kernel/sync/kcompletion/tests/host/Cargo.toml
```

The harness mounts the production `src/completion.rs` directly and uses the
real `intrusive-collections` 0.10.2 implementation. Only `kspin::SpinNoIrq` is replaced with a
host mutex; the kernel-test registration macro is suppressed. Host cases live
outside `src` and are mounted by path, matching the repository test layout.

The dependency uses the same `alloc`/`nightly` features as the kernel workspace;
its const list constructor requires the kernel's `RUSTC_BOOTSTRAP=1` setting
when running with the pinned stable compiler. This command supplies that setting
explicitly because it runs outside the kernel configuration. Miri runs use
`cargo +nightly-2026-03-08 miri test` with the same manifest.

A global allocator delegates to System while thread-local counters record
allocations and deallocations only inside `measure`. A positive control confirms
that both operations are counted. The measured paths require `(0, 0)` for:

- construction and already-ready completion/predicate waits;
- repeated waits with spurious returns and FIFO/token stealing;
- multiple waiter broadcast and predicate callback reentry;
- an actually parked cross-thread waiter and the notifier, measured separately;
- repeated cross-thread predicate publication;
- deferred publication/dispatch while another owner remains alive;
- stale deferred dispatch after reset.

Thread creation, host parker initialization, Arc construction, assertion output,
and final owner destruction are outside the measurement boundary. Per-thread
counters exclude unrelated parallel test activity. The probe verifies source
algorithms and their allocation behavior, not kernel IRQ/preemption semantics or
the ktask parker implementation. Deferred final-owner release is tested
for lifetime correctness separately because it may legitimately deallocate.

Guest tests are in `../../ut-tests/kcompletion_unittest.rs`. After adding the
crate to the kernel dependency graph and preparing the platform configuration,
run `make unittest UNITTEST_CRATE=kcompletion` from the repository root.

The suite also checks head/middle/tail cleanup on normal return and unwind,
notification racing stack retirement, competing token consumers, and payload
publication without an independent producer-to-reader release/acquire pair.
These lifetime/concurrency cases are not all allocation-measured; see
`../../docs/validation.md` for the coverage and limitations.

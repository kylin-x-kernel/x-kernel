# Queue partition storage preparation

Status: implemented; validation results and the existing UP test limitation follow.

## Objective and scope

Migrate `kworkqueue_ng::Queue.partitions` from `Box<[Arc<QueuePartition>]>`
to `KBox<[Arc<QueuePartition>]>` without adding a recoverable allocation
failure after default execution services have started. Queue routing,
admission, dispatch and shutdown behavior remain unchanged.

This proposal covers the fixed partition table, not dynamic workers,
diagnostic names, partition/source `Arc` allocations, color completions,
or existing `Vec` allocations. It does not make queue creation wholly
OOM-safe. Existing infallible allocations retain their fatal OOM policy.

## Linux reference and adaptation

Reference: local Linux tree `/root/linux-stable`, commit `8d3ae59288f1`.
The relevant implementations are in `kernel/workqueue.c`:

- `alloc_and_link_pwqs` allocates the per-CPU association table and PWQs,
  unwinding partially constructed associations on failure.
- `apply_wqattrs_prepare`, `apply_wqattrs_commit` and
  `apply_wqattrs_cleanup` separate replacement preparation, installation
  and old-reference cleanup. This is an ownership model to reuse, not
  the literal new-workqueue construction sequence.
- `__alloc_workqueue` can fail; its global list insertion precedes rescuer
  initialization, whose failure invokes destruction. `get_unbound_pool`
  can start a worker before installing the pool in its hash table.
  Linux therefore does not promise that all allocation precedes all
  execution or visibility.
- `workqueue_init_early` and `workqueue_init` separate early queue setup
  from execution availability. Our proposal does not enable early enqueue.
- `WQ_MEM_RECLAIM` supplies reserved execution capacity, not NOFAIL
  queue allocation. Its rescuer and dynamic-worker retry protocol are
  outside this migration.

X-Kernel already shares execution providers between queues. Reserve queue
metadata before starting default providers; bind it after providers exist.
Do not import Linux GFP context assumptions into `AllocFlags` contracts.

## Integration interface

Add an opaque `PreparedQueue` in `kworkqueue_ng`, exposed through the
existing `service` integration module, not the ordinary queue interface.
Its private fields own a validated `QueueConfig` and exactly one
`KBox<[MaybeUninit<Arc<QueuePartition>>]>` of matching length. It is neither
cloneable nor independently configurable after reservation.

Proposed interface shapes:

```rust
PreparedQueue::new(config: QueueConfig) -> KResult<PreparedQueue>
PreparedQueue::new_with_flags(config: QueueConfig, flags: AllocFlags)
    -> KResult<PreparedQueue>
service::bind_queue(prepared: PreparedQueue, service: impl FnMut(...))
    -> Arc<Queue>
service::bind_queue_with_diagnostics(prepared: PreparedQueue, ...)
    -> Arc<Queue>
```

These are interface sketches, not compilable declarations. The existing
provider closure types and diagnostic parameters remain unchanged.
Binding consumes the prepared object and never reserves another partition
table. Do not retain a config-taking binding wrapper that allocates implicitly.
Dropping an unused preparation releases its config and table, without
creating sources, registering diagnostics, or stopping/joining workers.

`new` uses `KERNEL`: reclaim-safe, sleepable task context, outside IRQ,
preemption-disabled and non-sleepable RCU regions, without locks needed by
reclaim/I/O. `new_with_flags` inherits the selected allocator contract;
`NOWAIT` only restricts this reservation, not later binding. It grants no
NMI or bounded-latency guarantee. Production uses `KERNEL`; test fixtures
that intentionally disable preemption use explicit `NOWAIT` reservation.
Binding retains its existing sleepable provider contract regardless of flags.

## Construction and publication

### Default queues

1. Check the current execution context and build all three validated configs.
2. Reserve all three partition tables. On any failure, drop earlier
   preparations; neither BH preparation nor normal worker startup has run.
3. Prepare BH services and start normal services using existing rollback.
   Their failures also drop the unconsumed queue preparations.
4. Bind the three queues against these providers, preserving the existing
   immutable boot-CPU coverage checks. No new recoverable failure is added
   between successful normal startup and registry installation.
5. Publish the complete `Services` through the existing short
   `NoPreempt` + `Once` installation. No reservation or binding occurs there.
6. Duplicate initialization or occupied softirq actions still require the
   existing explicit stop/join outside `NoPreempt`; preparation does not
   replace this lifecycle rollback.

### Independent queues

Check sleepable context, service readiness and complete CPU coverage, in
that order, before reserving the table. Then bind and return the queue.
Production registry reads borrow immutable, permanent services without
holding a registry lock. A reservation failure leaves those providers and
their dispatch queues unchanged. Preserve the existing validation error
precedence; do not reserve ahead of these checks.

During binding, `Arc::new_cyclic` provides a `Weak<Queue>` to partition
sources. `PartitionDispatch::create_source` creates detached sources; it
does not notify, enqueue demand or execute work. No new source publication
is permitted during construction. Diagnostic registration is different:
the directory may expose weak records before the queue is returned.
Do not claim that binding has no externally observable side effects.

## Initialization and destruction invariants

Use a private initialized-prefix guard inside the queue module, not a new
public generic allocator interface. It exclusively borrows the reserved slice
and owns the initialized count; the storage owner outlives the guard.

- Config CPU order is immutable, and table length equals that CPU count.
- Slots below the count contain exactly one initialized `Arc`; remaining
  slots are uninitialized. Write a slot before incrementing the count, with
  no intervening operation that can panic or fail.
- On early drop, destroy exactly the initialized prefix, then let the
  uninitialized-slice owner free storage. Never drop uninitialized slots.
- Only a complete prefix may transfer storage through `assume_init`.
  Disarm the guard exactly once when transferring ownership; successful
  queue destruction subsequently drops every partition reference once.
- A partial partition/source value not yet written remains an ordinary
  local owner and is cleaned up normally. No partially initialized table
  or queue escapes the constructor.

This guard is required even though production binding has no new `Result`
failure: dropping `MaybeUninit<Arc<_>>` alone leaks an initialized prefix
when a constructor unwinds. Kernel panic/abort is not a supported recovery
path, and this guard does not promise worker teardown after a fatal panic.
Expired weak diagnostic entries may remain until registration/inspection
prunes them; cleanup is not a byte-for-byte directory rollback.

## Errors and compatibility

Reservation returns the existing `KResult`, preserving `InvalidInput` for
invalid flags and `NoMemory` for allocation failure. Add
`QueueCreateError::Allocation(KError)` and `InitError::QueueAllocation(KError)`
with conversions implemented on the respective error types. Existing
configuration and provider-startup error variants keep their meaning.

Ordinary queue constructors remain fallible with their existing result
types, but gain an allocation error variant. Integration callers must pass
`PreparedQueue` rather than `QueueConfig` to bind functions. This is an
internal Rust interface change, not a userspace ABI change. Audit exhaustive
error matches and all binding call sites before implementation is complete.

## Design review conclusions

The following alternatives were rejected during source-based self-review:

| Alternative | Reason |
| --- | --- |
| Allocate in the current bind helper and add `?` | Adds failure after normal workers start, requiring new lifecycle rollback |
| Use `NOWAIT` everywhere | Does not solve post-start failure cleanup and unnecessarily restricts ordinary callers |
| Keep config and reserved storage as separate arguments | Allows CPU count/order mismatch and splits ownership invariants across callers |
| Drop a partially filled raw `MaybeUninit` slice directly | Leaks initialized partition references |
| Claim binding is allocation-free or wholly private | Existing `Arc`, diagnostic names and directory registration contradict that claim |
| Add generic fallible slice initialization to KBox now | Not necessary for this fixed-table migration; keep the initialization guard local |

The design is suitable for implementation provided the ordering and guard
invariants are enforced. This is not an implementation safety approval;
the actual unsafe transfer, test seams and updated callers require review.

## Implementation and acceptance plan

Touch `kworkqueue_ng` construction/service integration and `kworkng`
orchestration/errors/tests. Update both crates' design/security documents
and interface rustdoc when implementing. No workerpool redesign is planned.

Required tests must exercise the production reservation/orchestration seams:

- Inject reservation failure at each of the three default table positions;
  assert no provider preparation/start or installation occurs. Release all
  earlier preparations. Do not substitute a separate test-only ordering model.
- Preserve invalid-context, unavailable-services and missing-CPU precedence
  without calling the reservation operation.
- Verify unused preparations never invoke a provider binding callback.
- Exercise the actual prefix guard at zero, partial and complete counts;
  tracked owners drop once. Explicitly dropping a partial guard does not
  require guest panic recovery. Test successful ownership transfer separately.
- Verify table order matches config, success reuses reserved storage, and
  binding performs no second table reservation.
- After independent reservation failure, existing queues still execute work
  and a subsequent queue creation succeeds.
- Retain duplicate-install/action-conflict stop/join tests and startup-prefix
  rollback tests. Do not weaken them to accommodate the migration.

Run configured build, Clippy, unit-test Clippy and relevant guest tests on
x86_64, AArch64, RISC-V and LoongArch, including SMP/UP and stall-check on/off
coverage. Prepare platform defconfig first; report unsupported/skipped
combinations explicitly. Record actual validation results separately from this plan.

## Validation results

Local validation on 2026-11-03 used each platform's qemu_defconfig. Build,
normal Clippy and UNITTEST=y Clippy passed on all four architectures.

| Configuration | Guest selection | Result |
| --- | --- | --- |
| x86_64 default, SMP 4 | kworkqueue_ng,kworkng | 218 passed |
| AArch64 default, SMP 4 | kworkqueue_ng,kworkng | 218 passed |
| RISC-V default, SMP 4 | kworkqueue_ng,kworkng | 218 passed |
| LoongArch default, UP | kworkqueue_ng::queue,kworkng | 141 passed, 5 remote/multi-partition cases ignored |
| x86_64 SMP 4, snapshot/watchdog/stall-check enabled | kworkqueue_ng,kworkng | 218 passed |

The stall-check configuration also passed build and both Clippy modes. Its
first QEMU launch encountered an occupied vhost-vsock CID before guest boot;
retrying with VSOCK_CID=23184 passed. Configuration was restored to the x86_64
default after validation. Changed Rust files pass the pinned nightly formatter;
git diff --check passes.

The full LoongArch selection did not pass: the unchanged
`diagnostics_record_unittest::busy_record_skips_without_consuming_warning`
expects try_lock to reject a held lock, but kspin's non-SMP implementation
sets is_unlocked to true. That test, StallRecord and kspin are unchanged from
the base commit. The narrower selection excludes diagnostics, not failed
migration tests; all five new preparation/cleanup tests passed on every
architecture. This migration does not repair that independent UP assumption.

The production graph selects `system` tests. Replaceable registry/action
fixtures in services_unittest.rs were adapted but were not executed in these
runs. Failure injection covers the production preparation seam, not global
allocator OOM or nested Arc/name allocation failure. The unsafe prefix transfer
was source-reviewed; no new Miri or panic-unwind run is claimed.

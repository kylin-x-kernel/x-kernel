# khal — Design

This document covers the IRQ trap-entry adapter, crate test layout, and page-table
storage and mapping-reference adapters. Other HAL subsystems retain their own
interface contracts.

The NMI facade also exposes the platform's `shutdown_periodic_nmi`: a local,
fallible reusable-source shutdown, distinct from terminal `quiesce_nmi`. HAL
does not broadcast it, free KIRQ handlers or interpret queue submission as
completion. The source owner coordinates those steps.

The PMU facade forwards `shutdown_overflow_irq` and `restore_overflow_irq` to
that source owner through `PmuInterruptIf`. These task-context operations own
global PMU retirement/republication in the platform provider, not HAL. The local
NMI operation and terminal stop remain separate APIs with different lifetimes.

## Positioning

`khal::irq` is the IRQ trap-entry adapter. The generic IRQ core
lives in `kirq`; external callers should no longer use the generic IRQ API through `khal::irq::*`.

This module now owns only trap-entry responsibilities:

- registering the `IRQ` / `NMI` trap handlers;
- establishing the `NoPreempt` execution constraint before entering `kirq`.

## Scope

```text
arch/khal/src/
├── irq/
│   ├── mod.rs       # adapter helper exports only
│   ├── manager.rs   # trap handlers
│   └── nmi.rs       # khal::irq::configure_nmi (per-CPU interrupt line promotion, delegated to the platform NmiMechanismIf)
└── lib.rs           # khal::nmi / khal::pmu mechanism-neutral facade (re-export kplat)
```

Generic IRQ state, descriptors, dispatch, Line-owned NMI actions, lifecycle
hooks and `IntrManagerIf` belong to `arch/kirq`. `khal::nmi` is a
mechanism-neutral facade; mechanism details belong to `kplat::nm_irq` and its
platform implementations.

`khal::cpu_hwcap()` is the CPU-capability interface this crate provides to the
rest of the kernel: `kexec` calls it while building a new process's auxiliary
vector and publishes the result as `AT_HWCAP`. The calling direction is
kexec → khal → kcpu; khal does not detect capabilities itself, it asks `kcpu`
for the intersection the online CPUs advertise and restates it in the
architecture's own encoding.

## Architecture

```text
kcpu trap dispatch
    |
    `-- khal::irq
            |-- #[register_trap_handler(IRQ)]
            |-- #[register_trap_handler(NMI)]
            |-- NoPreempt guard
            `-- kirq::{handle_irq, handle_nmi}
```

Ordinary IRQ API call paths should go directly into `kirq`, not through the `khal::irq` facade.

NMI mechanism facade:

```text
watchdog / perf consumers
    |
    `-- khal::nmi (re-export kplat::nm_irq)
            |-- early_init / late_init   # mechanism probing + per-CPU enablement
            |-- mode / info              # runtime mechanism query (Pseudo / Hardware / None)
            |-- enable_periodic_nmi      # periodic NMI source (currently PMU)
            |-- quiesce_nmi               # park the local periodic source on the terminating path
            `-- khal::irq::configure_nmi # per-CPU interrupt line promotion
```

## Firmware and device-tree transport (`khal::firmware`)

`khal::firmware` owns the handoff data the firmware leaves behind: the boot
command line (`cmdline`), the validated device-tree blob (`dtb_bytes`) and the
board's requested log level (`dt_log_level`).

Role split, and why the level vocabulary does not leak into this layer:

- `khal::firmware` **provides** the raw property value and nothing else: it knows
  the firmware transport (`of::chosen_log_level`) but not what a level is;
- `klogger` **owns** the level vocabulary and validates the string;
- `kruntime` consumes both: `resolve_boot_log_filter` asks
  `dt_log_level()` first and hands the value to `klogger::set_filter_spec`, so an
  invalid property is reported and the resolver falls through to the command line
  and the Kconfig default instead of silently muting the boot log.

Data flow: `/chosen/x-kernel,log-level` → `of::chosen_log_level` →
`khal::firmware::dt_log_level` → `kruntime::resolve_boot_log_filter` → `klogger`.
Validating in `khal` would duplicate the vocabulary and split the single source
of truth, which is why the accessor returns the unvalidated `&'static str`.

## Firmware-node publication (`khal::firmware::fwnode`)

`khal::firmware` also publishes the backend-neutral firmware root after `of`
has validated and installed the DTB. `fwnode_root()` returns
`Option<FwNode<'static>>`; today it wraps `rs_fdtree::DtBackend`, and ACPI or
static backends can publish the same contract later. Publishing the root is
allocation-free: the backend is a borrowed tree plus an uninitialized lazy
index, and only the first indexed node access (after allocator setup) builds
the index.

Consumers above this module use the root instead of reaching into `of` for
firmware walks. `khal::firmware::devices::for_each_compatible` iterates
`FwNode::descendants`, matches `firmware_ids()`, and requests the primary MMIO
resource and the first IRQ declaration. The IRQ declaration is returned as
`FwIrqSpec` (controller node plus complete binding cells); this layer does not
decode cells and reports a malformed declaration as an error instead of "no
interrupt". `FirmwareDevice` also carries the neutral `FwNode`, so the
bus/device layer can request resolved DMA facts (`dma_window`,
`dma_effective_coherency`) without depending on a concrete parser. This layer
does not cache those facts: it keeps only the node handle, avoiding a second
expression of the same firmware truth. Mask, ancestor translation,
coherency-default and IOMMU policy remain outside `khal`.
Controller drivers own cell meaning through their registered domain
translators. Platform bring-up passes the same root to GIC discovery. The PCI
`interrupt-map` walk remains in `of` until PCI legacy INTx migrates.

The lazy index means device enumeration must not run before the allocator
exists. Early console and PMU discovery use allocation-free
`FdtNode::irq_spec(0)` directly instead of `for_each_compatible`.

## Calling constraints / execution context

- `irq_handler()` and `nmi_handler()` may only be called by the architecture trap dispatch.
- Both handlers first create a `kspin::NoPreempt` and then call `kirq`.
- `irq_handler()` calls `kirq::handle_irq()`; the hardirq lifecycle enter/exit and the
  IRQ completion order are the responsibility of `kirq`.
- `nmi_handler()` calls `kirq::handle_nmi()`; the NMI raw-hwirq dispatch semantics are the responsibility of
  `kirq`.
- `khal::irq` stores neither IRQ registry state nor NMI handler ownership;
  both remain in the single `kirq` core.
- `khal::nmi::early_init()`: called once after GIC initialization and before any NMI is enabled (platform
  early_driver_init); it probes the mechanism and writes the runtime `NmiMode`.
- `khal::nmi::late_init()`: called once each by the primary core's `prepare_interrupts` and by every secondary core's `prepare_local_interrupts`
  (per CPU); in hardware mode it enables `SCTLR_EL1.NMI` / ALLINT here.
- `khal::nmi::enable_periodic_nmi(period_ns, cb)`: called per CPU; returning `false`
  means this CPU cannot be armed (mechanism unavailable or resource conflict), and the consumer should degrade explicitly.
- `khal::quiesce_nmi()`: to be called only from the terminal halt path; it pins the current CPU with `NoPreempt`,
  and stops the armed local periodic source while keeping the callback and the IRQ configuration. It is a no-op when there is no `nmi_pmu` source or the local
  source is uninitialized, and must not re-arm after the call.
- `khal::irq::configure_nmi(hwirq)` delegates CPU-local preparation through the
  platform to KIRQ. Initial preparation requires a registered disabled source;
  repeated preparation rechecks admission without rewriting live attributes.
  Shared sources are unsupported. False means rejected/failed preparation,
  including an unavailable mechanism or a closing Line; it does not enable IRQs.
- `khal::nmi::mode()`: returns `NmiMode::{Pseudo, Hardware, None}`, for consumers
  (such as the watchdog) to decide whether to enable NMI-dependent features.

## Concurrency model

The `khal::irq` adapter has only a short-lived `NoPreempt` guard and holds no global IRQ state.
IRQ shared-state locking, NMI RCU ownership and lifecycle hooks are maintained
inside `kirq`.

After normal IRQ completion, the adapter suspends the architecture exception
marker, releases its local guard without a pending-work check and invokes
`kiface_task_api::TaskIrqExitCheckpointIf`, defined by kiface-task-api and provided by ktask.
Guard release preserves outer credits and IRQ state; cleanup and
scheduling checks belong to that single explicit entry. This is an explicit
scheduler checkpoint under the entry IRQ mask, not an ordinary preempt-enable
operation. Outer guards still exclude scheduling. The marker resumes on the
interrupted task's stack; dedicated NMI dispatch never invokes this checkpoint.

## Design decisions

### Why `khal::irq` is kept

Trap handler registration depends on `kcpu::excp::register_trap_handler`, and the HAL adapter must establish the `NoPreempt`
execution constraint before entering `kirq`. Therefore `khal::irq`
remains, but no longer carries the generic IRQ API or the x86 MSI-X vector helper.

### Why the x86 MSI-X helper is not in `khal::irq`

MSI-X vector allocation, APIC destination selection and MSI message composition
belong to the IRQ core/backend boundary. Currently `kirq::MsiBackendIf` defines the backend contract,
implemented by `arch/x86-apic`; `khal::irq` no longer exposes APIC ids or raw CPU vectors.

### Why `IPI_IRQ` does not enter `kirq`

`IPI_IRQ` is a constant generated from Kconfig. `kirq` must stay independent of `kbuild_config`, so
IPI callers use `kbuild_config::IPI_IRQ` directly and call `kirq::notify_cpu()`.

## Crate-level guest test layout

Guest tests live in the crate-root `ut-tests/` directory and are mounted at
their original source modules using `unittest::path_mod_test!`:

| Source mount | Test file | Module | Cases |
|---|---|---|---|
| `src/percpu.rs` | `ut-tests/percpu.rs` | `tests_percpu` | 1 |
| `src/paging.rs` | `ut-tests/paging.rs` | `tests_paging` | 2 |
| `src/paging.rs` | `ut-tests/paging_page_refs.rs` | `tests_page_refs` | 7 |
| `src/mem/mod.rs` | `ut-tests/mem.rs` | `tests_mem` | 3 |
| `src/time/mod.rs` | `ut-tests/time.rs` | `tests_time` | 4 |
| `src/time/epoch.rs` | `ut-tests/time_epoch.rs` | `tests` | 7 |
| `src/firmware/devices.rs` | `ut-tests/firmware_devices.rs` | `firmware_devices_unittest` | 4 |

These 28 cases require `cfg(unittest)` without additional feature, architecture
or user gates. The huge-leaf lifetime case runs serially and compares allocation
usage before unmap and after synchronous retirement, with no independent owner
masking early release. `inactive_table_destruction_releases_its_live_data_hold`
additionally leaves a real managed data leaf installed in a never-activated
tree and checks that Drop restores the allocator usage baseline. All four
supported platform unittest builds select them.
CPU identity tests require initialized CPU state. Independent epoch fixtures do
not freeze the active kernel's global clock. Explicit cfg gates outside the
mounts prevent non-UT builds from resolving the runtime test macro; they do not
alter the dependency graph or replace boot-helper UT flag cleanup.

## Monotonic time across explicit suspend

`time/epoch.rs` maps backend counter ticks into logical MONOTONIC ticks.
`time/suspend.rs` exposes explicit freeze/rebase operations. HAL provides
`ClockSourceIf` over the platform's `CounterSourceIf`, so consumers outside HAL,
including RCU, observe the same logical domain. Frequency and conversion ratios
remain unchanged; sleep compensation stays in ktime.

The state machine is Running -> Suspended(token) -> Running. Suspend saves the
logical tick boundary; frozen reads return it without touching the backend.
Resume replaces counter_base and preserves logical_base. Running time is
logical_base + wrapping(counter - counter_base), with saturating accumulation.
Counter progress, stoppage or reset inside the frozen interval is excluded.
Backend continuity while running remains a platform obligation. Frequency
changes are unsupported. The initial identity mapping preserves the existing
backend origin until the first formal suspend.

Read-side atomic fields and sequence validation avoid global locks and shared
writes. Counter sampling occurs inside the sequence check. Writers acquire an
even sequence with AcqRel CAS, publish an odd sequence, issue a release fence,
update atomic fields, then release-publish the next even sequence. Readers
acquire the initial sequence and recheck after an acquire fence. Mixed snapshots
are discarded without non-atomic Rust data races. Identities never wrap; suspend
reserves room for resume, and rejected operations preserve the active token.

These control operations require a coordinated PM boundary: park other CPUs,
stop devices and timer programming, and quiesce watchdog/NMI clock readers.
Writers briefly mask local IRQs/preemption. NMI clock reads must be quiesced
because they can interrupt an odd-sequence writer and deadlock. Busy waits and
timeouts cannot progress while frozen. This layer does not implement that PM
coordination or a host protocol. Tokens are non-copyable and do not resume on
Drop; the PM owner retains them until explicit resume succeeds, including abort
paths. A trusted external source must account for any omitted frozen interval.

Clock events receive relative intervals through ClockEventIf::arm_timer_after.
HAL preserves arm_timer(MonotonicInstant), subtracts the logical current time,
and disarms instead of arming while frozen. ARM, x86 and LoongArch program
countdowns; RISC-V adds the remaining interval to its raw counter before SBI
programming. Logical absolute timestamps must not reach raw compare registers.
NoPreemptIrqSave pins the sample-through-program sequence to the current CPU;
PM excludes concurrent programming during epoch transitions.

A future PM owner begins ktime accounting, disarms events on each CPU and
quiesces execution, freezes the monotonic clock, sleeps, restores counter access,
rebases MONOTONIC, settles confirmed sleep through ktime, restores local event
hardware and re-arbitrates deadlines on every CPU before releasing execution.
Suspend disarms the coordinator's event again after sampling, since legacy
repair may have rearmed it. LocalTimerReprogramIf supplies existing deadline
arbitration; PM decides when each CPU's hardware and software state are ready.

The Linux reference is kernel/time/timekeeping.c timekeeping_suspend/resume:
save the boundary and rebase cycle_last on resume. This change adapts freeze,
rebase and event-domain boundaries only, not complete clocksource/syscore,
device PM or RTC sleep estimation. The isolated epoch tests cover counter
resets, progress, wrap, repeated cycles, stale tokens, identity exhaustion and
read retries; they do not prove real S3 or multiprocessor stop coordination.

## Local event restore capability

`time::restore_local_timer` forwards ClockEventIf::restore_local_timer to the
selected backend. It restores event configuration with a stopped countdown;
clocksource state and software deadline arbitration are separate. The task
timer service owns the pause gate and final arbitration. Platform PM must
restore controller access/routing and quiesce competing programming first.


## Page-table storage lifetime references

`PagingHandlerImpl::alloc_frame` obtains kalloc's single-page `PageRef` range from the global
single-page allocation API and transfers one hold to `page_table` as a PA.
`dealloc_frame` restores and consumes that exact transfer with
`PageRef::from_raw(pa, 1)`. The allocation adapter drops the length-one component
of the generic (PA, page-count) raw pair; this table-storage interface remains
PA-only and is separate from the data-reference `(PA, bytes)` provider.
The original `UsageKind` records only allocation accounting; final `PageRef::drop`
returns the page and records the release directly in kalloc. Borrowed root entries
do not own their source table pages and do not restore or release those holds.

The table owner must end hardware use and complete required translation
retirement before destroying owned storage. The existing AArch64 user-ASID
invalidation path in `PageTable64::drop` remains the final reclamation gate:
failed completion preserves table frames and all unrecovered managed data
transfers. Reference counting retains storage; it does not replace address-space
exit, remote-CPU coordination or TLB shootdown. Managed data leaves use the
separate provider below.

`paging_page_refs.rs` tests the allocator adapter: it checks the handler's raw
reference transfer and uses the architecture's real `PageTable` to cover tree
destruction and borrowed root entries. It uses the real `PagingHandlerImpl` and
global `PageRef`; the generic page-table `TestHandler` has no such ownership or
reclaim protocol. Test tables are never activated in hardware, so these cases do
not validate generic page-table algorithms or TLB shootdown.

## IRQ Entry Contract

The private `irq::manager::{irq_handler, nmi_handler}` callbacks implement kcpu's unsafe trap callback
types. Architecture entry supplies the captured source and ordinary IRQ mask;
HAL adds a preemption credit and forwards that proof to KIRQ. KIRQ completes
claims and finishes its context before the normal IRQ adapter performs the
scheduling checkpoint. Dedicated NMI return has no such checkpoint. These
adapters do not turn arbitrary task-context calls into valid hardware entries.

## PTE-owned data mapping references

`PagingHandlerImpl::MappingReference` is `kalloc::PageRef`. The adapter reports
`phys_addr()` and `page_count() * PAGE_SIZE_4K` for the selected handle range.
A compound view can retain a larger allocation, but only its selected exact
range is transferred for one aligned base or huge leaf. The caller transfers a
dedicated mapping hold through the primary `map(reference)`,
`map_sized(reference, size)` or `replace_if_same(reference)` API and keeps any
object hold independently.

The provider bridges the existing raw `PageRef` protocol:

| Provider operation in `src/paging.rs` | Transfer |
|----------------------------------------|----------|
| `mapping_reference_into_raw(reference)` | Consume `PageRef::into_raw()` and convert its base-page count into the exact selected byte length, returning `(PA, bytes)`. |
| `unsafe mapping_reference_from_raw(PA, bytes)` | Convert the validated leaf byte length back to base-page count and restore exactly one outstanding transfer with `PageRef::from_raw(PA, pages)`. |
| `unsafe mapping_reference_try_clone_from_raw(PA, bytes)` | Borrow a live managed leaf's exact transfer with `ManuallyDrop<PageRef>` and acquire a distinct hold with `try_clone_ref()`; leave the source transfer untouched. |

The transfer and recovery conversions do not change reference counts. Recovery requires
exclusive ownership of the same unconsumed exact transfer; an arbitrary PA,
metadata lookup or raw mapping is insufficient. No new `kalloc` dependency is
introduced in generic `page_table`.

The leaf's normalized PA, size and architecture-private managed category retain
that transfer. Category is outside public `PagingFlags`, survives retained-leaf
flag/address updates and is never created by raw installation. Bit selection and
audit belong to the architecture PTE implementations. A copied PTE or snapshot
duplicates no hold. There is no permanent per-leaf owner index or heap allocation
for recording an admitted hold.

The page-table transaction validates the exact range and finishes all fallible
preparation before transferring and publishing the leaf. On clear/replacement
it first captures the old leaf, then records invalidation and restores the old
managed hold into bounded retirement storage. Recovery acquires no new hold.
Only confirmed completion releases it, with full invalidation for huge leaves.
Retirement still keeps 16 holds inline and may fallibly spill to at most 256;
spill OOM or a full batch attempts an intermediate drain while retaining the
recovered hold. Unconfirmed completion keeps holds and a full retry; unresolved
guard destruction or an unstorable hold stays pinned. There is no asynchronous
reaper. Synchronous revocation still uses `try_finish()` and handles its typed
completion error; retained lifetime does not establish revoked permissions.

Inactive-tree destruction recovers every owned base/huge managed leaf exactly
once before descriptor-frame release, skipping borrowed roots and honoring the
final AArch64 user-ASID completion gate. `copy_from` borrows source transfers
without cloning them and checks the full destination root range for managed
leaves in owned subtrees before any destructive overwrite. Sources must outlive
borrowers, and destination managed leaves must first be unmapped and retired.

PA-only callers mechanically use `map_raw`, `map_region_raw`, `remap_raw` and
`replace_if_same_raw` for externally owned backing. File Folios now own PageRef,
file PTEs acquire independent managed holds, and mremap clones managed source
holds with map_existing. Anonymous COW targets retain their existing external
raw ownership; anonymous, VMM and device migration remain separate work. A raw
install still transfers no mapping hold. Raw replacement cannot change a managed leaf's normalized PA;
same-PA updates preserve category, and permission clearing retires its old hold.
Existing raw release paths using `finish()` can still release backing after an
unconfirmed shootdown. The adapter does not make those paths safe: they need a
separate ownership transfer or retained backing on `try_finish()` error. The
provider grants no content-access or device-completion authority.

The PageRef regression cases retain real allocator inventory/reuse and
exact/compound lifetime checks, using `map`, `map_sized` and
`replace_if_same` for reference-consuming mapping transactions and explicit raw
names for PA-only fixtures. Inactive test tables do not verify hardware
shootdown or prove an architecture's private category-bit selection.

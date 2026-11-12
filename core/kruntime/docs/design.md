# kruntime — Design Document

## Network Startup Owners

Late-init task context calls the fallible `knet::init_network`. Device owners
are reserved with KERNEL before NIC notification setup and global publication.
Failure terminates at this explicit fatal-boot boundary because configured
network initialization has no recovery/error-return path here. This policy is
not inherited by namespace construction or other runtime network APIs.

## IRQ Startup Ownership

The x86 runtime fixture `msi_lookup_interleaves_with_release_and_reallocation`
uses a real reader task and phase acknowledgements across 32 MSI ownership
cycles. It verifies stable lookups after admission and strict misses after
release while lookups also interleave with transitions. No device is programmed
to deliver MSI, and the reader is stopped and joined before result assertions.

Timer and IPI actions are registered disabled. Each CPU enables its own generic
timer slot before platform source activation; fixed IPI notification setup
enables that CPU's IPI slot. Publishing one shared action does not mark other
CPUs enabled. Source activation and CPU IRQ enable retain their existing order.

Late-init native-console handoff now reports preparation errors. Runtime logs
the error and keeps the existing boot-console route; it does not manufacture a
NOFAIL allocation or publish a partially prepared TTY. A later native-terminal
request can retry initialization. Ordinary devtmpfs creation propagates its own
console preparation failure separately.

Runtime owns the cross-subsystem ordering: install current, publish foundation
handlers, activate local sources, prepare IRQ-exit services, then enable CPU IRQ
delivery. The AP runqueue-ready and service-ready rendezvous serve different
consumers and remain separate. Neither a controller driver nor kirq can infer
that scheduler, clockevent and worker prerequisites have all completed.

`cpu_readiness.rs` owns per-logical-CPU interrupt readiness and provides
`kiface_cpu_api::CpuInterruptReadinessIf`. Primary and secondary startup publish
after local source activation, softirqd/helper setup and local irq_work setup,
immediately before ordinary CPU IRQ enable. Boot remains non-preemptible through
this publication. Each independent bit uses Release/Acquire; sampling during
startup may omit newly prepared CPUs, but never derives readiness from a count
or from firmware presence. There is no allocation or lock on the query path.
`SCHED_READY_CPUS` permits task enqueueing; `INITED_CPUS` completes the later
IPI/watchdog/PMU rendezvous. Neither count substitutes for this identity set.
In particular, the primary publishes before late-init enables fixed IPI
notifications; this set must not be used as an IPI transport-ready query.

This is the interrupt-readiness part of Linux's CPU startup ordering:
`arch/arm64/kernel/smp.c::secondary_start_kernel` and
`arch/riscv/kernel/smpboot.c::smp_callin` prepare local interrupt facilities and
publish online before enabling local IRQs. X-Kernel does not yet implement the
full hotplug/online lifetime protocol. The readiness set only grows at boot;
it does not promise immediate delivery or that every irqchip can address every
ready CPU. GIC target maps, PLIC contexts, IO-APIC destinations and LoongArch
parent routes remain controller-owned checks. PMU source setup must not use the
later all-services rendezvous as a prerequisite for its own routing setup.

Platform boot hooks select the hardware and invoke controller preparation and
local source activation. irqchip drivers own register programming and their
hardware readiness; timer drivers own clockevent programming. kirq owns generic
descriptor, action, dispatch and line enable state. It does not own an additional
global boot-ready state or the concrete controller drivers.

The responsibility split follows Linux's startup organization, not its exact
call sequence: `init/main.c::start_kernel` orders `early_irq_init`, `init_IRQ`,
timer/softirq setup and `local_irq_enable`; arm64's `init_IRQ` invokes
`drivers/irqchip/irqchip.c::irqchip_init`, while x86's `init_IRQ` invokes the
architecture interrupt-initialization hook. A common phase interface does not
require moving platform discovery or hardware implementation into the generic
IRQ core. X-Kernel retains its own current-installation and AP rendezvous needs.

Early console/controller discovery remains distinct from local source activation.
After GP and worker startup, the late-init thread invokes `khal::console::prepare_input`
before device discovery can construct N_TTY. The console provider owns optional
input setup and polled fallback; runtime only orders this phase. Validation and
coverage limits for controller startup are recorded in
[IRQ startup validation](irq-startup-validation.md).

## RCU Host Adapters

The kiface-irq-context-api provider in kirq reports the common IRQ core's context facts,
including the distinction between BH exclusion and serving a handler. It
preserves NMI-first queries and existing snapshot protection, without deciding
RCU scope admission, watching or cleanup eligibility.
kiface-cpu-api providers in kplat and kcpu supply CPU identity and exception presence. The scoped exception
snapshot holds the existing IRQ-only guard through RCU's admission assertions,
then restores the caller's mask before read-side execution. It does not add
preemption accounting or a scheduling checkpoint. The independent exception
query retains the architecture's own snapshot protection.

The softirq adapter binds RCU's action to the existing fixed Rcu vector and
raises it on the current CPU. It adds no action table, queue or executor. kirq
retains softirq exclusion, pending bits, restart/time limits and daemon fallback;
krcu retains its callback budget and decision to re-raise for eligible work.
RCU startup serializes registration with driver acquisition, preserving retry
after registration conflict.

Task startup consumes RCU's PreparedGpTask in the existing PID-less task entry
allocation, using the normal stack size and scheduling path. No extra boxed
closure or control registry is introduced. RCU owns the one-shot driver claim,
entry body and startup acknowledgment; the runtime only prepares and activates
the kernel task. Its temporary task reference may be dropped after activation:
the scheduler retains the runnable task. Both the caller's startup wait and the
GP task's own parking use the task-wait adapter, each bound to its own current.

The kiface-time-api provider in ktask owns the concrete Timer and lends a one-shot capability
on its stack. It translates monotonic nanoseconds to the timer deadline type,
preserves owner routing across migration and synchronously drains delivery on
return. The expiry function has no captured allocation; RCU's permanent service
receives it and publishes demand. Intervals, deadline retention after early
wakes, assistance and GP completion remain krcu policy. Service readiness is
published from inside the adapter body, after timer preparation.

The same integration module lends current task reader storage to krcu through
`RcuReaderStorageIf`. Its stack-local CurrentTask borrow spans the synchronous body,
without allocation or an owner-count update. It supplies no scope-admission or
cleanup policy; those remain in krcu. IRQ/NMI use the same storage adapter without
locks or changes to IRQ/preemption state.

Task preemption controls use kiface-task-api directly, provided by ktask: querying its actual count,
setting a request, running a normal checkpoint and deferring checks around one
synchronous body. Runtime no longer forwards these generic task methods. RCU
owns cleanup decisions; ktask owns count balancing and switch eligibility.

Synchronous completion waiting now uses kiface-task-wait-api directly, implemented by
ktask with a stack-local `CompletionWait`, rather than an RCU runtime adapter.
It lends infallible `wait_for_completion` operations. Preparation
validates sleepability before calling the RCU body; preparation and waiting do
not allocate or use poll registrations. RCU keeps callback completion and
retirement ownership, including its existing callback/drain Arc allocations.
TaskParkingIf, also provided by ktask, retains `PreparedTaskWait` waker cloning and
park operations for startup acknowledgment and GP progress. Wake hints merely
allow those callers to check their predicates again. Neither adapter returns
a preparation error after invoking its body.

RCU now owns its drain serialization mutex through kmutex, including marker
installation and the completion wait. There is no runtime drain adapter or
runtime-owned RCU lock. Initialization remains lazy before marker installation.

`src/rcu.rs` provides krcu's static notification contract through existing kipi
delivery, or local Rcu softirq raising without IPI support. It owns no reader,
grace-period, callback queue or completion state. RCU publishes work/demand and
chooses which CPU needs notification; runtime supplies the transport only.
`kfeat/ipi` enables both runtime's provider and the krcu contract. IPI delivery
rejects invalid or absent CPUs with BadState; without IPI only the current CPU
is accepted. Service initialization remains a caller prerequisite: notification
success does not validate readiness, and IPI requests before notification
enablement remain pending for boot drain.

## Local irq_work Startup And Probes

Primary interrupt initialization and each secondary's IRQ-service setup install
the local irq_work provider before enabling NMI users. This is independent of
kworkng readiness and creates no threads. Unsupported platforms remain unready
with a CPU-local warning. If the configured runtime exposes an available NMI
mechanism, missing local irq_work is a fatal startup error before NMI consumers
are enabled, rather than a later last-reference failure. GICv3 preparation logs
the failed SGI reservation, registration, classification or enablement step.
Tests submit 193 self-freeing nodes per CPU under an IRQ mask, verify no inline
execution, then hand a callback to a retained normal Work and actually sleep.

The `entry/unittest` feature enables `kruntime/irq-work-test`; its optional
AArch64 irq-driver and device-tree direct dependencies are disabled in ordinary
runtime builds. The NMI-enabled, watchdog-disabled live fixture owns each CPU's
otherwise unused periodic PMU source. Before arming it, the fixture masks the local PMU line
using its DT-resolved IRQ number: hardware NMI attributes cannot be changed on
an enabled line. It re-enables delivery after successful configuration.
It publishes 129 preallocated nodes from a real NMI while
holding the GIC lock, checks zero ordinary callback execution before unlock,
then waits for ordinary IRQ and worker completion. It quiesces its source after
the probe, masking its local PMU line first so hardware NMI cannot interrupt
counter shutdown between disabling the counter and its overflow IRQ. Source
configuration/callback storage remains installed but inactive
for the guest lifetime under the existing one-way quiesce contract. It never
replaces a watchdog-owned source. Watchdog-enabled builds exercise the ordinary
probe without taking over their NMI facility.
When the configured NMI mechanism falls back to `None` on the actual hardware,
the live probe reports a skip instead of treating this supported fallback as a
test failure; available NMI still requires local irq_work readiness.

The same owned-source probe now drops prepared DeferredArc references in NMI:
last strong without Weak, last strong followed by last Weak, and a last Weak
whose payload already completed on a worker. It verifies immediate upgrade
failure, zero source allocations and sleepable destruction after IRQ unlock.

## Deferred finalization integration probes

Production rust_main installs preparation once after primary allocator/per-CPU/
trap setup and before scheduler initialization, without creating tasks or waiting
for workers. Successful workqueue initialization binds execution later.

In unittest builds, rust_main prepares and retires 193 BootPayload objects
immediately after allocator initialization, with per-CPU/trap support ready but
before scheduler/current Task and worker setup. Weak upgrade already fails and
the destruction counter remains zero. The later registered test waits for all
sleepable worker destructors and one child prepared/retired by the first
destructor, checking that normal preparation is ready before backlog execution.
Additional probes run under the primary startup NoPreempt guard and on each AP
before current Task setup and under its startup NoPreempt guard: 194 + 2 per AP.
Production boot creates no probe objects. The unsafe construction entry belongs
to kfinalize::service; provider installation belongs to kworkng::runtime.

The unittest-only workqueue `deferred` module prepares DeferredArc owners in task
context and releases their last references from a task, a real timer hard IRQ,
and a normal-priority BH callback. Source checks distinguish hard IRQ from an
IRQ-masked task. A caller-CPU allocator snapshot covers strong/weak release under
one IRQ/preemption guard. The payload verifies sleepable normal-worker context
and actually sleeps. A BH callback is a submission source, not a finalizer.
The probe does not claim NMI support, global allocation freedom or latency bounds.

Shared finalizer Work may retain unfinished ownership on another CPU. Submission
therefore marks its IRQ-guarded source CPU and checks that destruction does not
run inline on that CPU; a remote executor may legitimately finish before return.
A zero global destruction count during submission is not a valid assertion.

Boot-owned worker services remain initialized for the whole integration run;
the probes install no service hooks and wait for payload field references to
disappear. Timing comparisons belong to the opt-in kworkng release benchmark,
not this coverage-instrumented integration test suite.

## RCU Carrier Startup

After the all-CPU service rendezvous, `late_init_main` calls
`krcu::init_service` and waits sleepably for the GP task to prepare its own
wait and timer. This precedes workqueue and driver startup. It does not enable
ordinary RCU callback admission or advertise completed observation integration.
Startup errors stop boot; an unprepared GP task is not reported as ready.
`krcu` owns service state, the GP task, progress timer and Rcu softirq action;
`ktask` supplies generic execution support. The `krcu_core::RcuContextIf`
provider is linked before this rendezvous and service startup, so early
watching and reader enrollment do not depend on GP readiness.

## System Workqueue Startup

`late_init_main` calls `kworkng::runtime::init` after `init_cpu_services` has
observed completion from every boot CPU, before driver discovery and network
initialization. This sleepable phase creates per-CPU normal managers, normal/BH
default queues and both Tasklet/High actions. Startup failure stops boot instead
of exposing drivers without workqueue services. There is no legacy fallback or
implementation-selection Kconfig. AP initialization creates ksoftirqd but does
not create legacy worker pools.

The legacy workqueue crates, scheduler accounting provider and procfs stress
trigger are removed. Only prepared workqueue services are linked.

`KFEAT_WORKQUEUE_BENCH` optionally routes the production entry, after upper
runtime initialization, through `run_boot_benchmarks` instead of userspace.
The kworkng runner uses already published services and powers off on completion.
The default configuration and unit-test entry do not execute this path.
`KFEAT_RCU_TORTURE` uses the same post-initialization handoff for the RCU
ownership stress runner, with a separately configured duration. Kconfig excludes
simultaneous selection with the workqueue runner. Neither workload belongs to
normal boot or default UT. The runtime-owned runners in `benches/rcu/` create
tasks, arrange load, collect timings and verify results using only public
`krcu` APIs. They do not access RCU's internal state. Runtime powers off on
successful return. See [runner instructions](../benches/rcu/README.md) for
configuration and pass criteria.
Owned RCU fixtures and the read/torture benchmarks prepare KBox payloads with
NOWAIT before publication. Tests rejecting replacement inside a read scope
therefore do not add a sleeping allocation to that scope. Opt-in benchmarks
require every fixture to preserve their complete sample/release ledgers and
stop on fixture allocation failure; this is not a production OOM policy.
`KFEAT_RCU_BENCH` selects the independent callback/synchronous-wait runner through the
same handoff. Kconfig excludes both stress runners while it is selected.

## Positioning

RCU integration tests in `ut-tests/rcu_service_startup.rs` and
`ut-tests/rcu_synchronize.rs` verify task/timer carrier preparation and public
wait semantics against boot's permanent services. They own their temporary
tasks, timers and interruption state. Private RCU completion/ownership
fixtures remain in `krcu`; production APIs do not expose their private state.
For a focused kernel regression run, select both crates with
`UNITTEST_CRATE=krcu,kruntime` so internal and integration checks execute together.
`kfeat` assembles `smp` and `ipi` across runtime, RCU and the corresponding
task/notification providers. Runtime features select only its local paths.
RCU does not enable task features on behalf of its host; its own SMP feature retains shared-lock
support independently of that assembly.
`ut-tests/rcu_callback.rs` owns callback integration fixtures for real task,
timer hardirq and softirq paths, plus controlled NMI/unwatched rejection. It
checks returned ownership through caller-side cancellation/invocation and
records the callback's node address rather than inspecting private handle
fields. Burst completion is a public progress check; exact budget boundaries
remain core queue tests. No fixture may release an accepted probe before
its completion publication.
`ut-tests/rcu_context.rs` checks the host's exception/interrupt classification
and IRQ/preemption state preservation across public read scopes. Its controlled
context markers are not hardware interrupt delivery tests.
`ut-tests/rcu_drain.rs` checks public wait-context rejection and the boundary
between callback completion and downstream task completion. It submits through
`PreparedCallback` and holds a task-only gate until drain returns, then joins
the task. Concurrent drains wait behind the host gate even after callback
destruction; repost tests close their producer gate and wait for in-flight
submissions before draining. No producer lock spans public callback submission.
Marker counters and reuse checks remain internal RCU tests.
`rcu_read.rs`, `rcu_cleanup.rs`, `rcu_service.rs`, `rcu_owned.rs` and
`rcu_implicit.rs` cover actual host boundaries, migrated readers, per-CPU
callback execution and guarded ownership. The runtime-local `rcu_support`
helper retains the distinction between current-task operations and remote
atomic observations. Unittest-only RCU snapshots expose cleanup demand and
whether drain markers are installed. They cannot clear demand, unlink a reader,
mutate marker ownership or certify a GP. The marker check runs while the fixture
holds the RCU-owned drain gate and verifies that blocked drain callers install nothing.
Isolated startup, deadline and completion policy tests remain in krcu and use
its host contracts without depending on concrete task or IRQ implementations.

Startup distinguishes a published run queue that accepts tasks from a CPU whose
services are ready to execute them. After scheduler initialization, each AP holds
NoPreempt before publishing SCHED_READY_CPUS, through fixed-notification drain,
ksoftirqd creation, IRQ enable and optional watchdog/PMU initialization. It releases
the guard before run_idle. Requests remain pending rather than being discarded.
IRQ masking alone is insufficient because internal lock release can check preemption.

After scheduler initialization returns, the primary likewise protects IRQ service setup and the creation and activation of the late-init task, then releases the guard before entering normal scheduling. The driver/filesystem/network phases of late_init_main may wait for I/O and are not covered by this protection. The protected startup region must not explicitly yield, block, or wait for a task that this CPU has not yet run; initialization steps added later must maintain this constraint.

The unit-test startup hook, once all AP run queues are ready and before the primary has published INITED_CPUS, delivers a task and a reschedule request to each CPU in turn; the task checks the AP service-ready marker. The result is retained until the test executes and does not depend on busy-wait duration; after the test task exits, no runtime hook or stack borrow remains.

Runtime enables fixed notifications after `kipi::init` on the primary, then
releases AP service initialization. Each AP enables notifications before creating
its local services and publishing completion. The notification entry enables the
line, publishes ENABLED, and drains early requests. `SCHED_READY_CPUS` still means
run-queue registration, not IRQ readiness; static reason bits retain early requests.

`kruntime` is the **runtime orchestration layer** of x-kernel: after platform boot code hands control to the kernel, it is responsible for primary/secondary CPU initialization order, subsystem assembly, and the jump to the system initialization entry (`SystemInitEntry`, provided by the `entry` crate). It implements no concrete device drivers or syscalls; instead it chains `khal`, `memspace`, `ktask`, `kdriver` and others together in fixed phases.

Target audience: kernel developers who need to understand the boot chain, SMP barriers, or add initialization hooks in this crate.

## Background

x-kernel separates **boot** (`kernel-boot`, platform `platconfig`) from the **runtime** (memory, scheduling, drivers, userspace init). The boot layer hands primary/secondary CPU control to the runtime through `BootInfo` and single-implementation `kiface` entries; `kruntime` provides the implementations of these entries (`rust_main` / `rust_main_secondary`).

The boot topology follows the FreeBSD `proc0` model: the boot/current task is a scheduler-internal task (`Internal` identity, not in the ordinary PID allocator). `rust_main` activates a PID-less late-init kernel thread that runs all late init (drivers/fs/net/SMP, etc.) and calls `SystemInitEntry::enter()` once all CPUs are ready. `entry` **spawns a brand-new PID 1 user task** (init), after which the late-init thread exits. Init does not "transform in place"; it is a newly spawned task that follows the same standard `new_user` + `publish_user_task().commit()` path as fork.

Key design: the late-init thread and all background workers spawned during late init (network poller, TTY, watchdog, etc.) are PID-less `Internal` kernel threads (corresponding to FreeBSD `kthread_add`) and do not enter the ordinary PID space. Init's allocation is therefore the first ordinary allocation in the root namespace, and PID 1 is the only fixed PID at boot.

Application logic (e.g. `entry::runtime::init_runtime`) lives in the `entry` crate; assembly of the initial user process is handled by `posix-process::spawn_init_process`, so that `kruntime` does not depend back on high-level services.

## Scope

Source files involved:

```
core/kruntime/
├── src/
│   ├── lib.rs              # primary-core rust_main, log adapter, interrupt/allocator, memory-region log
│   ├── mp.rs               # SMP secondary-core boot, TLB unit test
│   ├── cpu_readiness.rs    # Per-CPU interrupt-readiness publication and query
│   ├── init_setup.rs       # .init_array constructor table traversal
│   └── lang_items.rs       # #[panic_handler]
├── Cargo.toml
└── docs/
    ├── design.md
    └── security.md
```

## Architecture

```
kernel-boot (assembly / MMU / BootInfo)
        │
        │  PrimaryKernelEntry::enter(boot_info_ptr)
        ▼
┌───────────────────────────────────────────────────────────────┐
│  kruntime::rust_main (primary CPU, boot task)                 │
│    khal / memspace / kalloc / klogger / backtrace             │
│    init_scheduler / init_interrupt                            │
│    create PID-less late-init bootstrap thread                 │
│    activate late-init, then park PID 0 boot task              │
└───────────────────────────────────────────────────────────────┘
        │
        ▼
┌───────────────────────────────────────────────────────────────┐
│  late_init (Internal, PID-less bootstrap thread)              │
│    SMP / IPI / CPU services                                  │
│    wait for INITED_CPUS == nr_cpus                            │
│    drivers / fs / net / init_cb                               │
│    SystemInitEntry::enter() ─────────────► entry crate        │
│    entry spawns and activates fresh PID 1 init task           │
│    return, then exit                                           │
└───────────────────────────────────────────────────────────────┘
        │
        │  boot_ap → SecondaryKernelEntry::enter(logical_cpu_id)
        ▼
┌───────────────────────────────────────────────────────────────┐
│  kruntime::rust_main_secondary (secondary CPU)                │
│    percpu / trap / memspace secondary / scheduler secondary / IPI │
│    wait for primary -> local services -> publish -> idle      │
└───────────────────────────────────────────────────────────────┘

Lateral wiring (link time, not boot order):
  lib.rs            ──provide──►  kiface_logging_api::LogSinkIf / LogContextIf
  lib.rs            ──provide──►  kernel_boot::PrimaryKernelEntry
  mp.rs             ──provide──►  kernel_boot::SecondaryKernelEntry
```

| Component | Responsibility |
|------|------|
| `rust_main` | The sole full initialization path of the primary CPU; entered via the `PrimaryKernelEntry` provider |
| `rust_main_secondary` | Secondary CPU initialization; entered via the `SecondaryKernelEntry` provider; finally `run_idle` |
| `mp::start_secondary_cpus` | Prepares the stack for each AP, calls `boot_ap`, and waits for the AP to register its run queue (precondition for safe cross-CPU spawn) |
| `init_setup` | Runs the `register_init` callbacks in the `.init_array` link section |
| `lang_items` | On panic, prints a backtrace and calls the bare platform endpoint `khal::power::platform_power_off` (bypassing the SMP stop hook to avoid an IPI-stop deadlock when panicking while holding a lock) |

## Boot Flow (Primary CPU)

`rust_main` is divided into the following phases (matching code order):

```
BootInfo*
    → firmware::init / percpu::init_primary / init_trap
    → mem::init
    → init_allocator (resident physical-page metadata, page inventory, then byte heap)
    → register_boot_console_runtime_region (optional MMIO UART)
    → memspace::init_memory_management
    → early_driver_init
    → klogger + backtrace (linker symbols _stext/_etext)
        (Log filtering: `init_klogger(configured_log_level())` fixes the build
         level — the console default and the floor of the log filter, which a
         runtime request may raise but never lower. `resolve_boot_log_filter()`
         then takes the first accepted source: an explicit `loglevel=` /
         `quiet` / `debug` command line, then /chosen/x-kernel,log-level, then
         Kconfig `LOG_FILTER`. A rejected source is reported on the console
         through `kprintln!` and the resolver falls through to the next one; a
         command-line scan continues past a rejected `loglevel=`, so a later valid
         one on the same line still applies.)
    → prepare_interrupts
    → init_scheduler
    → init_interrupt (softirq runner, timer/IPI/PMU handlers,
                      activate_local_interrupts, ksoftirqd, enable_local_irq)
    → create late-init bootstrap thread (Internal, PID-less)
    → activate late-init thread
    → boot task (PID 0) block forever

late-init thread (Internal, PID-less):
    -> [smp] start_secondary_cpus (all AP run queues registered)
    -> init_cpu_services:
        -> [ipi] kipi::init, enable_fixed_notifications
        -> [watchdog] init_primary
        -> [pmu] enable_irq
        -> INITED_CPUS += 1 (release AP service initialization)
        -> wait until is_init_ok() (all AP services prepared)
        -> [smp] mark_all_cpus_started (enable TLB shootdowns)
    -> krcu::init_service, kworkng::runtime::init
    -> khal::console::prepare_input (optional IRQ input, polled fallback)
    -> [fs] init_device_discovery
    -> init_drivers (cross-CPU services are now available)
    → [char] console handoff
    → entropy::init
    → init_setup::init_cb (filesystem/subsystem self-registration)
    → [display/input] fb / input
    → [fs] fs_boot (root namespace, virtual filesystems)
    → [fs9p] fs_boot host-share mount
    → [net/vsock] knet
    → finish_allocator_init, log_memory_regions
    → [vsock_tipc_bridge] start_vsock_bridge (depends on cross-CPU scheduling, so it must come after the barrier)
    → SystemInitEntry::enter()
        (entry; spawns a brand-new PID 1 user task init, then returns)
    → late-init thread exits (task_entry automatically calls ktask::exit)

init (PID 1, a newly spawned user task):
    → the scheduler automatically activates the user page table on switch-in (switch_page_table_root)
    → run_user_thread_loop → enter user mode
```

### Boot RAM handoff

`init_allocator()` collects the HAL's `MemFlags::FREE` ranges into the caller's bounded `heapless::Vec<PhysAddrRange, 256>`. `page_meta::init(&mut ranges, map_storage)` normalizes in place and deducts the backing region, returning only the PageMetaTable; the mapping callback only converts the PA of the actual backing region into a stable writable VA and does not keep mapping information on each physical range.

The runtime serially publishes the unified PFN lookup service, then calls the existing global_init with the first non-empty remaining range in ascending PA order and hands off subsequent non-empty ranges one by one with global_add_memory. The buddy allocator selects ranges in registration order; preferring low addresses avoids a DMA32 allocation first hitting a range above 4 GiB and failing outright. VA conversion is done at these two actual use sites.
The input container capacity is part of runtime/HAL wiring and is not a public region abstraction of page-meta. 256 is the number of input entries before normalization; the current backend accepts at most 64 merged discontiguous segments, and the existing buddy allocator likewise registers at most 64 regions. These two limits count different things.

page-meta owns storage planning, indexing and its own reservation; the allocator owns the bitmap, free inventory and heap; the runtime is responsible only for the physical list, mapping capability and handoff order. Coverage includes the deducted backing pages and currently describes only whole pages of the input FREE RAM. The covered page count is derived from the segment table via covered_pages(), and the boot log's metadata_pages is computed locally as the covered page count minus the pages remaining before handoff; neither is stored as a table field.
On error the metadata table is not published; the input may already have been normalized. If the allocator fails after the metadata table is published, boot stops as fatal and the whole transaction is not retried.

### PID 1 Boot Convention

The boot/current task and idle task created by `ktask::init_scheduler()` both have scheduler-internal identities (`Internal`/`Idle`) and do not call `kidentity::allocate_root_pid_handle()`. The late-init thread is also `Internal`, and all background workers spawned during late init (`ktask::spawn` goes through `new_pidless_kthread`) are likewise PID-less `Internal`. Therefore, when `SystemInitEntry::enter()` creates init, the first ordinary PID allocation in the root namespace must yield PID 1.

This ordering fixes the early PID semantics of the root namespace as:

```text
PID 0   = internal tasks of the boot/idle/swapper kind (do not go through the ordinary allocator)
PID 1   = the init user process (a brand-new user task spawned by the late-init thread)
PID >=2 = user processes forked later or explicit Linux-visible tasks; ordinary kernel workers do not consume a PID
```

Init no longer "transforms in place": the late-init thread constructs a brand-new `User`-identity task through `SystemInitEntry::enter()` → `posix-process::spawn_init_process`; its runtime is made ready once at `new_user` construction (`UserRuntimeSlot::ready`), published and activated via `publish_user_task().commit()`, and the page table is written automatically by the scheduler at first switch-in through `switch_page_table_root`. There is no "after-the-fact installation" mechanism such as `install_user_runtime`, an empty-slot state machine, or `activate_current_user_page_table`.

### Global Readiness Barrier `is_init_ok`

```rust
INITED_CPUS.load(Acquire) == kcpu_id_map::nr_cpus()
```

- The primary is the first CPU to increment `INITED_CPUS` with Release ordering,
  after preparing its IPI, watchdog, and PMU services. APs wait for a nonzero
  Acquire load before completing their local service setup.
- Each AP increments once after fixed notifications, ksoftirqd initialization,
  local IRQ enable, and optional watchdog/PMU setup. Workqueue services are
  initialized centrally only after every CPU reports completion.
- The primary waits for the runtime-discovered `nr_cpus()`, not the configured
  `NR_CPUS` capacity, before device discovery and driver initialization. The final
  Acquire load observes the Release RMW sequence of CPU completions.
- CPU readiness is not driver, filesystem, or network readiness. The bridge and
  `SystemInitEntry::enter()` remain after their subsystem initialization.

An AP releases its startup preemption guard after publishing completion and then
enters idle. No AP service initialization may wait for primary driver startup or
for a task that cannot run until that guard is released. Otherwise the primary
and AP would wait for each other. APs never call `SystemInitEntry`.

The unittest startup fixture holds AP completion publication until the primary
has entered its service wait. It observes completion immediately before RCU
initialization, exercising the rendezvous without a timer or runnable-task
dependency. Normal builds do not install this handshake.

### NMI Initialization Sequence

The NMI mechanism is probed by the platform and enabled per CPU; `kruntime` is responsible for calling the `khal::nmi` interfaces at the correct initialization phases:

- `early_driver_init` (after GIC initialization completes) → `khal::nmi::early_init()`: probes the GIC version and FEAT_NMI, records the runtime mechanism (`Pseudo` / `Hardware` / `None`), and prints the `NMI mode: ...` boot log.
- Primary `prepare_interrupts` and each secondary's `prepare_local_interrupts` → `khal::nmi::late_init()`: per-CPU enablement. Hardware mode turns on `SCTLR_EL1.NMI` / ALLINT here; pseudo and the degraded mode (`None`) need no per-CPU register operations.
- `[watchdog] init_primary` in the late-init thread (and `init_secondary` before a secondary's idle) calls `init_nmi_watchdog`: when `mode() == None` it prints "hard lockup detection disabled" and skips arming; otherwise it arms hard-lockup detection with the PMU periodic source via `khal::nmi::enable_periodic_nmi`.
- PMU overflow dispatch is registered independently by the `pmu` feature init (normal IRQ or NMI fallback) and is decoupled from the watchdog, so perf events remain available on degraded platforms.

## Boot Flow (Secondary CPUs)

```
SecondaryKernelEntry::enter(logical_cpu_id)
    → percpu::init_secondary, init_trap
    → memspace::init_memory_management_secondary
    → prepare_local_interrupts, init_scheduler_secondary
    → SCHED_READY_CPUS += 1  (for start_secondary_cpus to wait on; RQ is registered, cross-CPU spawn is safe)
    → [ipi] kipi::init
    -> wait until INITED_CPUS != 0 (primary services prepared)
    -> activate_local_interrupts (before notifications can reprogram the timer)
    -> enable_fixed_notifications
    -> init_softirqd_current_cpu
    -> enable_local_irq
    -> [watchdog] init_secondary
    -> [pmu] enable_irq
    -> INITED_CPUS += 1 (publish local service completion)
    -> release startup preemption guard
    → run_idle()  (never returns)
```

`start_secondary_cpus` waits for each AP's `SCHED_READY_CPUS` publication after
scheduler initialization before starting the next AP. This permits cross-CPU
task placement but does not permit assuming that local services are ready.
The primary subsequently completes `init_cpu_services` before entering drivers.

## `.init_array` and `register_init`

`init_setup.rs` maintains the linker section `.init_array`:

- `_SECTION_PLACE_HOLDER`: ensures that even an empty image has `__init_array_start` / `__init_array_end`.
- `init_cb()`: late-init, after driver probe and entropy initialization and before filesystem namespace construction, steps through the section by pointer and calls each `extern "C" fn()`.

Subsystems can register early init in this section through `#[register_init]` from `util/macros`, without modifying the `kruntime` source list.

## Known Limitation: loongarch64 QEMU Direct Boot Has No ACPI Tables

`make justrun` (loongarch64) boots directly with `-kernel`. The EFI system table forged by QEMU's `loongarch_direct_kernel_boot` contains only three configuration table entries: memmap/initrd/DTB. The ACPI tables (`virt_acpi_setup`) are exposed only through fw_cfg files (`etc/acpi/tables`, `etc/acpi/rsdp`) and are **never written into guest RAM**; they are reachable only when booting through UEFI firmware (the firmware reads fw_cfg, copies the tables into RAM, and installs an RSDP entry in the EFI config table). Therefore, under direct boot, `rsdp=0` and dormant table parsing are expected behavior rather than a defect (Linux cannot obtain ACPI under the same boot method either). Power-off falls back on the DTB's `syscon-poweroff` declaration (although direct boot has no ACPI tables, QEMU's DTB declares the GED sleep-control register address and the S5 write value). x86-64 (SeaBIOS) is unaffected.

## `kiface` Wiring

| Interface | Implemented in | Purpose |
|------|----------|------|
| `kiface_logging_api::LogSinkIf / LogContextIf` | `lib.rs` | Log output to `khal::console`, annotated with CPU/task ID |
| `kernel_boot::PrimaryKernelEntry` | `lib.rs` | Primary CPU enters `rust_main` from the boot layer |
| `kernel_boot::SecondaryKernelEntry` | `mp.rs` | Secondary CPUs enter `rust_main_secondary` from the boot layer |
| `kruntime::SystemInitEntry` | `entry/src/main.rs` | Enters the system-level init policy layer once the runtime is ready |
All are link-time exactly-one single implementations, not runtime registrations.

## Cargo Features

| Feature | Effect |
|---------|------|
| `smp` | Secondary CPU startup, `rust_main_secondary`; cross-module SMP configuration is enabled uniformly by `kfeat/smp` |
| `ipi` | Depends on `kipi`; IPI interrupt handling |
| `fs` / `fs9p` / `net` / `vsock` / `display` / `input` | Driver and subsystem initialization (via `kdriver`); concrete filesystem features only link their self-registering implementations |
| `rtc` | Prints wall-clock time at boot |
| `watchdog` / `watchdog_hardlockup` | Watchdog initialization on primary/secondary CPUs; `watchdog_hardlockup` additionally enables NMI-driven hard-lockup detection (`watchdog/nmi`) |
| `pmu` | PMU overflow interrupt; under the `pmu` feature `kruntime` independently registers overflow dispatch (`khal::pmu::register_overflow_dispatch`), decoupling perf from the watchdog; together with `watchdog_hardlockup`, the PMU line can be promoted to a periodic NMI source |
| `arm-timer-resume-fixup` | Checks for AArch64 counter regression on every read and sends the deferred recovery notification from the timer interrupt |
| `rootfs-secondary-block` | Uses the second block device as the root filesystem backend |

All are off by default; they are enabled by the defconfig of `entry` / `kfeat`.

## Design Decisions

### Why `SystemInitEntry` Instead of Depending on `entry` Directly

`kruntime` is depended on by `entry`; if `kruntime` also depended on `entry`, a cycle would form. `SystemInitEntry` turns the handoff into a single-implementation `kiface` interface: `kruntime` owns the call contract and `entry` provides the policy implementation, while avoiding reliance on bare symbol names and `extern "C"` calls.

### Why Only the Primary Runs `init_cb`

`.init_array` callbacks assume that the interrupt subsystem is registered and that the multitasking application phase has not yet begun; placing them after `init_interrupt` and before `SystemInitEntry::enter()` is close to the timing of C runtime constructors, but with the call point explicitly controlled by the kernel.

### Log Adapters Live in `lib.rs`

`LogSinkIf` and `LogContextIf` must take effect through `kiface` provider linkage before `klogger::init_klogger`; keeping them in the same file as `rust_main` avoids initialization-order problems.

## Boundary with QEMU / Real Hardware

`kruntime` itself is **platform-independent**: the `BootInfo` contents are filled in by `kernel-boot` plus the platform crate. Differences between QEMU virt and boards (e.g. raspi) lie in the boot layer and platform prepare/activate hooks; the phase table after entering `rust_main` is the same.

## Redundancy and Overload

- **Redundancy**: none (single-path primary init).
- **Overload control**: not in this crate; scheduling and memory pressure are handled by `ktask` / `kalloc`.

## ARM64 counter recovery dispatch

With `arm-timer-resume-fixup`, the timer IRQ handler dispatches pending counter-recovery notifications before processing task timers. Clock reads only repair shared counter state and rearm CPU-local registers; IPI delivery is deferred until caller-held clock-read locks have been released. The dispatch runs without the counter lock, does not sleep, and requires initialized interrupt routing. It does not reconstruct elapsed host sleep time.

DMA mapping attributes no longer pass through a runtime adapter: kiface-mm-api owns
DmaMappingMode and DmaMappingIf; memspace directly implements the transition.

## IRQ Management Integration

Runtime does not provide an IRQ-specific mutex. KIRQ owns its `kmutex::Mutex`
and the RAII guard across membership validation, action withdrawal and drain.
`ktask` supplies the generic mutex's scheduling operations, and `ksync` reexports
the same implementation. Timer/IPI registration uses
`kirq::try_register_percpu_startup`, rather than inferring startup from masked
IRQs. The controller must resolve each fixed source to PerCpu flow. Registration
requires disabled local slots and never enables delivery; each CPU activates its
own sources later. A wrong flow is a fatal boot error, not a generic-registration
fallback.

`init_interrupt` explicitly prepares the fixed timer/IPI identity descriptors
through `prepare_fixed_interrupts` before installing either handler. These
descriptors belong to platform/runtime lifetime and are reused by AP activation;
preparation does not configure or enable the controller. Handler installation
still uses the startup entrance, with source activation later. This is an
incremental separation of descriptor preparation from action ownership, not a
claim that all legacy KIRQ registration entrances already require prior mapping.
The startup fixture records descriptor identities and PerCpu classification before handler publication
and requires this observation before local source activation. It does not
measure hardware masking or prove every caller's mapping lifetime.

Generic mutex contention and guard migration tests live beside the scheduler
provider in `ktask/ut-tests/task_api.rs`. Lifecycle membership, stale-Line
selection and dispatch drain remain KIRQ responsibilities with separate tests.

`ut-tests/irq_management_lifecycle.rs` combines real task acquisition and drain
waits with KIRQ's unittest-only retained in-flight fixture. It observes both the
remover and same-Line requester blocked, verifies another Line can progress,
then explicitly releases the in-flight operation and checks completion and
handler destruction. The fixture retains the original Line and balances its
dispatch accounting without fabricating a controller claim or running a
sleeping IRQ handler. This is lifecycle integration coverage, not hardware IRQ
delivery or controller-completion coverage. Handshake stalls are bounded by the
external QEMU runner; elapsed time does not decide correctness.

The NMI shutdown case combines KIRQ's synthetic `NmiShutdownProbe` with real
`kipi::run_on_cpu_sync` calls on two initialized boot CPUs. The source owner
serializes restart, retains its shutdown handle in remote captures, and recovers
unique ownership only after those captures are destroyed. A failed local
teardown keeps the original action installed until the unfinished CPU succeeds.
Controller acknowledgements are injected; successful IPI execution is not
treated as physical source drain or completion of the surrounding IPI claim.

The same lifecycle fixture tests directory/Domain reuse with task and interrupt
RCU borrows on another CPU. The interrupt variant uses KIRQ's software hardirq
entry and nested NMI query fixture, not a physical interrupt injection. A
Release/Acquire handshake keeps the old reader alive across withdrawal and
same-key publication; callback drain happens only after reader exit. This
checks retained identity, successor lookup and eventual reclamation without
requiring the writer to wait for IRQ delivery on the reader CPU.

## System power management

System suspend/resume coordination belongs to `core/kpm`, independently of
runtime bring-up. See `../../kpm/docs/design.md` for the transaction, platform
contract and cooperative task-freezing sequence. Runtime does not own PM
admission or expose a compatibility PM module.

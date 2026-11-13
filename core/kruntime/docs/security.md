# kruntime — Security and Reliability Analysis

Unit-test entry preparation failure terminates the fixture at its assertion
boundary; it is not a production OOM recovery protocol. NOWAIT preparation in
boot and retained-reader handshakes only constrains the entry allocation, not
task stacks, names, captured resources or their destruction. No new entry is
activated after its preparation fails. This does not promise recovery of an
already active multi-stage reader fixture after a fatal test assertion.

Benchmark task entries are prepared with KERNEL permissions in the sleepable,
reclaim-safe coordinator, not in the child task's future read or IRQ context.
Complete preparation precedes activation of every concurrent group. An entry
allocation error therefore cannot strand an activated task behind an unopened
gate or an unset stop flag. Dropped prepared task references release captures
through the existing deferred finalizer; cleanup is not guaranteed synchronous
with the error return. Successful guest runs do not prove global OOM recovery.

RCU benchmark record allocation occurs before publication in sleepable,
reclaim-safe task context. The batch fixture reserves storage before creating
a record whose destructor decrements its live count; failed reservation cannot
underflow that count. Invocation and cancellation reconstruct only the original
KBox allocation through its zero-offset node projection, exactly once. The
softirq destructor remains nonblocking. Local failure injection checks owner
reservation only, not nested allocation, global OOM, or production recovery.

Network device-owner reservation failure is fatal only at the configured boot
network boundary. It occurs before global stack publication and NIC scheduler
attachment; ordinary namespace callers receive NoMemory. Late init provides
sleepable reclaim-safe context; this does not authorize KERNEL allocation from
early IRQ-disabled startup. Nested allocation and poller failure policies remain
unchanged, and boot allocator OOM is not injected by the tests.

The x86 MSI lookup/reallocation fixture checks software publication and removal
using task interleavings, not physical interrupt drain or device DMA teardown.
Its acknowledgements work on UP and SMP; an external QEMU timeout diagnoses a
stalled handshake. The fixture adds no production boot policy.

Local timer activation must successfully enable the current CPU's KIRQ slot
before the runtime publishes IRQ readiness. Failure is fatal at boot. Shared
action installation alone cannot authorize delivery on an unprepared CPU.

The analysis includes `src/cpu_readiness.rs`, which stores independent boot-only
readiness bits. Only the two runtime startup paths publish their current CPU,
under startup preemption exclusion after trap/controller preparation, source
activation and IRQ-exit service setup. Acquire observations pair with Release
publication of that CPU's preparation. Failed preparation cannot reach the
publication point. Querying the set does not certify remote execution, current
IRQ enablement, controller addressability or safe reclamation. No runtime CPU
removal is supported by this contract; hotplug would require coordination with
IRQ retargeting before withdrawal, not just clearing readiness.

Native TTY callback allocation failure during late-init handoff is recoverable:
runtime retains boot-console access and logs a warning. The failed initializer
does not attach an input notification or poison subsequent retries. This does
not guarantee a working `/dev/console` until devtmpfs preparation succeeds.

## RCU Ownership Validation

RCU ownership fixtures use prepared KBox values. Runtime tests retain delayed
readers across replacements and check each displaced allocation is released
only after its grace period, including interrupt-reader cases. Optional RCU
benchmarks require allocated fixtures and stop rather than report partial data
on owner failure. Neither path changes production RCU service startup policy.

## Foundation IRQ Startup

Controller preparation runs before scheduler installation with ordinary CPU
delivery masked. Runtime checks timer and (when configured) IPI registration
before local source activation. A rejected registration is fatal startup failure:
runtime must not activate local clockevents, enable CPU IRQ delivery or launch
APs. Already installed handlers are not rolled back before reclamation services
exist. Platform preparation/activation errors likewise stop mandatory bring-up.

Local activation precedes softirqd task placement on the primary and pending
fixed-notification drain on APs; both can program a clockevent. AP runqueue and
service-ready rendezvous remain distinct. NMI feature setup stays in controller
preparation; NMI consumers still require the later irq_work readiness contract.

This phase split does not yet relocate early console mapping/registration or
its UART source control. Those legacy operations remain an explicitly separate
console-input change, not evidence that all device sources obey this foundation
startup sequence. Boot observations test preparation, current/handler readiness
and IRQ exclusion at activation; they do not inject a controller failure.

## irq_work Initialization And Live Validation

Runtime initializes irq_work in allocator-ready, CPU-local startup before NMI
consumers. A failed/unsupported preparation does not publish readiness; callers
must handle NotReady rather than synchronously executing the callback in NMI.
Runtime startup rejects an available NMI mechanism without local irq_work on
each CPU, before enabling NMI consumers. Platforms without an available NMI
mechanism may continue without irq_work and emit a warning. GICv3 preparation
failures additionally report the failed operation. Direct NMI clients must still
obey the local readiness contract; diagnostic task owners remain owning references.
The test-only real-NMI probe is excluded when watchdog owns the periodic source.
Its nodes and Work captures are prepared before NMI publication and retained
until callback completion plus Work flush. It quiesces the test-owned source;
that source cannot be rearmed in the same boot. A host timeout must bound failed
hardware delivery or a genuine lockup; no test timeout proves latency bounds.

Deferred reference probes retain their containing KBox outside NMI until the
source's release/acquire completion handoff. NMI takes only the prepared fields;
it never frees the containing fixture allocation. Payload and metadata retirement
are separate, including late Weak after payload completion. This validates only
DeferredArc, not Task's outer standard Arc or arbitrary diagnostic accesses.

## RCU Carrier Preparation

The softirq provider delegates to kirq's fixed-vector registration. Conflict
cannot replace the old action, and installation neither raises pending work nor
calls the action inline. Dispatch must retain the ordinary softirq context and
CPU pinning. Local raising requires no scheduler/RCU lock because daemon wakeup
may enter the scheduler; the RCU action rechecks its own queues without such locks.

After accepting the prepared GP entry, task startup must preserve it through
normal task activation. It cannot substitute a different entry, execute inline,
duplicate driver ownership or return a recoverable failure that loses the
claimed driver. Existing task/stack allocation retains fatal OOM behavior.
The runtime does not infer readiness from successful activation.

The kiface-time-api provider in ktask owns scoped timer storage. Arming it requires initialized scheduler/timer services
and an IRQ-compatible expiry function. The runtime neither invokes expiry
inline on arm nor manufactures a quiescent observation. Cancellation and Drop
drain delivery before freeing timer storage; callers must not hold callback-needed
locks or cancel from that callback. The borrowed timer cannot outlive the body.

Task wait preparation is supplied directly by ktask through kiface-task-wait-api,
not by the runtime RCU adapter. It may reject a non-sleepable caller, but only before
invoking the RCU body. Afterward the adapter must retain the prepared wait for
the full synchronous operation, including early completion and migration.
CompletionWait acquires RCU's completion infallibly without allocation or signal
interruption. RCU retains retired ownership and callback/drain Arc allocations.
Startup and GP service waits retain the distinct PreparedTaskWait park/Waker
contract, whose hints cannot establish RCU completion.

Drain serialization requires sleepable task context and excludes recursive
drain calls. RCU owns the permanent kmutex and releases it only after marker
completion; no runtime drain adapter remains. Callers must not hold locks
needed by callbacks or GP progress. Neither callbacks nor the service acquire
this mutex; their marker accesses are synchronized by krcu independently.

The current-reader adapter uses initialized current storage only and retains its
borrow until the RCU body returns. Its CurrentReader certificate does not grant
permission to enter a read scope or unlink a reader. In NMI it relies on the
interrupt entry's retained current, and performs no locking or scheduling.

The static RCU notification adapter cannot allocate, wait or run callbacks.
IPI delivery does not wake tasks inline; the no-IPI local softirq path may wake
its daemon and is not usable while scheduler locks are held. RCU retains the
choice of safe calling paths and its no-IPI periodic fallback. A delivered
notification is never proof of reader exit, CPU quiescence or GP completion.

The unsafe `krcu::init_service` call follows the all-CPU service rendezvous,
which supplies its scheduler, timer and notification prerequisites. The boot
task sleeps until the GP task has prepared its own wait and timer. Failure stops
boot. This acknowledgment does not authorize ordinary callback admission or
replace the remaining idle/user/interrupt observation and context requirements.
The callback carrier is exercised only with controlled unpublished fixtures.
The permanently linked `krcu_core::RcuContextIf` provider supports early
watching and enrollment without claiming that timer/notification services or
the GP task are ready. Runtime owns the startup ordering, not the RCU arrays,
cleanup policy or service state, which belong to `krcu`.

## Workqueue Publication Boundary

Production startup installs preparation after primary per-CPU, trap and allocator
initialization, before consumers. Unittest new_integration probes cover primary
and AP startup, outside allocator locks, without waiting for reclamation before
worker binding. Each CPU must establish its own allocation/IRQ-lock prerequisites;
global provider readiness alone is insufficient. Hardirq/softirq/NMI construction
is not permitted, even after provider installation.

The default-off `workqueue-bench` feature exposes a boot test path, not an
untrusted runtime request. It intentionally suppresses userspace and powers off
after the kworkng workload. Production deployment must leave this feature off;
test automation must impose a host timeout for stalled waits or joins.
The independent default-off `rcu-torture` boot path has the same deployment and
timeout constraints. Kconfig excludes simultaneous selection of these runners.
Runtime owns the workload's assertions, worker lifetimes and observation
storage; `krcu` supplies the public reclamation guarantees being tested.
Runtime runs the workload after initialization and powers off on successful
return. A timeout must fail externally, never force reclamation of live records.
The default-off `rcu-bench` path has the same constraints and is mutually
exclusive with the stress runners in Kconfig. Measurement PASS confirms the
runner completed its checks, not that a performance threshold was achieved.

The protected asset is the kernel-lifetime `kworkng` service set and its per-CPU
execution providers. Publishing work before remote CPU services exist could lose
notifications or strand callbacks. The all-CPU completion barrier precedes
workqueue initialization, and drivers are exposed only after successful service
publication. Failure is fatal at startup; there is no partially operational
legacy fallback. Normal workers may sleep, while BH callbacks must remain bounded
and nonblocking. Legacy worker pools and BH actions are not installed.

IRQ allocation probes exist only in unittest builds. Their test-owned timers
are drained before work storage; producers stop before final synchronous work
cancellation. Allocation counters cover successful heap bytes on the guarded
caller CPU, not remote execution, allocation attempts or NMI.

The deferred-owner probe releases its slot lock before handing off payloads.
Timer delivery and pinned BH Work are drained before their captures are destroyed,
even on an expired bounded completion wait. Last-reference tests also release
their Weak control blocks inside the measured interval; only metadata may be
freed there. Payload Drop and its storage always run on the normal executor.
The BH callback only submits already prepared owners; it never performs the
payload's sleepable destructor or waits for its completion.

## Trust Model

The remainder of AP startup borrows the idle task's execution context, so switching early to a continuously runnable task
could postpone its initialization indefinitely. NoPreempt covers the span from before the run queue ready notification is published until this CPU's
required services are ready; exiting the inner SpinNoIrq can only restore the outer preemption-disable count and must not switch away early.
The timing of enabling the global IRQ is unchanged, the guard does not prevent IPIs from being recorded/handled, and the final release consumes
need_resched through the existing path. No runtime ready-state check, global scheduler lock, or notification-clearing path is added.

The primary CPU protects only early service preparation and late-init activation; blockable late-init work must not also be wrapped in
the preemption-disabled region. Initialization extensions must not actively schedule inside the guard or depend on tasks that are not running to complete; NoPreempt
prevents preemption, but does not automatically make an illegal active wait safe. No unsafe is added and the EEVDF ordering is not changed.

Before the fixed IPI is enabled, the current scheduler, the normal IPI queue, the controller and the shared handler must be ready.
The primary CPU enables it through an IRQ-save critical section, while APs keep their original boot state with the global IRQ disabled; the enabling drain
only sets the reschedule flag and does not actively perform a preemption check or timer business callbacks. The static
reason bits must not be cleared during initialization, and the normal handler must not consume in advance requests for a CPU that is not yet enabled. CPU hotplug is not supported.

`kruntime` is the trusted orchestration layer between the platform boot layer and the kernel runtime. It does not directly parse syscall arguments or user pointers, but it receives the `BootInfo` passed in by the boot layer, installs the early memory, scheduling, interrupt and driver runtime environment, and calls `SystemInitEntry::enter()` after all discovered CPUs have completed runtime initialization.

```
kernel-boot / platform firmware
   │ BootInfo*, primary entry, secondary entry
   ▼
┌──────────────────────────────────────────────────┐
│ kruntime                                          │
│   rust_main                                       │
│     early runtime / scheduler / IRQ               │
│     spawn PID-less late_init thread               │
│   late_init_main                                  │
│     SMP / CPU services / INITED_CPUS barrier      │
│     drivers / fs / net / .init_array               │
│     SystemInitEntry::enter()  -> spawn PID 1      │
│   rust_main_secondary                             │
│     AP runtime init -> INITED_CPUS -> run_idle    │
└──────────────────────────────────────────────────┘
```

- **The boot layer / firmware** must provide a readable and semantically correct `BootInfo`, CPU topology, memory regions and an optional boot console MMIO description.
- **The linker script and registration macros** must provide valid `_stext` / `_etext` and `__init_array_start` / `__init_array_end`, and guarantee that `.init_array` contains only `extern "C" fn()` entries.
- The `kiface` exactly-one provider contract enforces unique implementations
  of `PrimaryKernelEntry`, `SecondaryKernelEntry`, `LogSinkIf`, `LogContextIf`
  and `SystemInitEntry` at link time.
- **The `entry` provider** is responsible for policy-layer startup: `SystemInitEntry::enter()` must publish and activate the PID 1 init task before returning.

## External Boundaries / Attack Surface

All external inputs to `kruntime` come from inside the kernel trust boundary, but errors are amplified into global failure early in startup:

| Boundary | Input | Risk | Current constraint |
|------|------|------|----------|
| `BootInfo` | CPU ID, memory table, boot console PA/VA/size | Wrong memory mapping, wrong CPU initialization, MMIO region out of bounds | `khal::firmware::init`, `khal::mem::init` and `memspace` are responsible for validation; the in-page offset of the boot console VA/PA is checked by `assert_eq!` |
| Boot RAM handoff | `FREE` PA/VA/size tuples | Misclassified or aliased RAM could overwrite live owners | Bounded heapless collection; `kalloc` checks arithmetic, overlap and linear mapping, while boot/HAL must establish ownership and mapping lifetime |
| Linker symbols | `_stext`, `_etext`, `.init_array` boundaries | Wrong backtrace range, constructor table out of bounds, arbitrary jump | Only the addresses of ZST symbols are taken; `.init_array` is constrained by the linker script and `#[register_init]` |
| SMP bring-up | present CPUs discovered via DT/ACPI, AP stacks, `boot_ap` | A secondary CPU using the wrong stack, an AP that fails to start causing the primary CPU to spin | Each AP uses its own boot stack; `SCHED_READY_CPUS` serial handshake (waits for the AP to register its run queue); global completion is determined by `INITED_CPUS == nr_cpus()` |
| IRQ and timer | Softirq runner and timer/IPI/PMU handlers | Interrupt delivery before handler readiness | The primary installs handlers before IRQ enable; APs enable local IRQs after ksoftirqd setup and before publishing service completion. Workqueue service publication follows the all-CPU barrier. PMU IRQ enable follows watchdog setup. |
| NMI mechanism | GIC version / FEAT_NMI probing and per-CPU enabling (`khal::nmi::early_init` / `late_init`) | Mechanism misdetection causes invalid register accesses, or the watchdog falsely reports a hardlockup | Gated by the platform `detect_mode()`; with `NmiMode::None` the watchdog disables hard-lockup detection at startup; all NMI register accesses are gated by readiness |
| Boot log filter (DT) | `/chosen/x-kernel,log-level` string, read by `khal::firmware::dt_log_level()` inside `resolve_boot_log_filter` | A malformed or hostile property could mute the boot log or install an unintended per-target filter | `klogger::set_filter_spec` validates the whole spec before applying it; a rejected value is reported on the console through `kprintln!` (the unfiltered path, because the level being installed is what would filter a `warn!`) and the resolver falls through to the command line and the Kconfig default, so logging is never left off by a typo |

This crate does not directly handle user memory, user-supplied pointers, DMA buffer contents or device register reads and writes; the related trust boundaries are located in `memspace`, `kdma`, `kdriver` and the specific HAL/drivers. It registers the fixed MMIO mapping metadata of the boot console, but does not access that MMIO directly.

## Unsafe Code Inventory

### Boot RAM transfer (`src/lib.rs::init_allocator`)

unsafe `page_meta::init(&mut ranges, map_storage)` is called before allocator users and before the secondary cores
start. Boot/HAL guarantees that the RAM in the physical list is exclusively owned, that the callback's p2v mapping covers the entire backing region,
and that the backing RAM and mapping remain valid permanently. The FREE filter itself cannot prove these facts.

page-meta deducts its own backing region in place and preserves the complete physical coverage. The runtime hands only the non-empty
ranges of the updated list to kalloc; the physical-page content mapping is not a fixed field of every covered range. The primary core calls the unsafe
publish serially, and commits to the lifetime of the borrowed static metadata.

The metadata service maintains the index, descriptor initialization and publication; kalloc maintains its own inventory and heap; the runtime
maintains the mapping capability and boot ordering. An initialization error is not published; the single failed reservation of with_reserved_storage is not deducted, and earlier successful reservations
and normalization results are not promised to be rolled back. Once the metadata table is published, an allocator failure stops boot, with no partial retry.

### Linker text boundary (`src/lib.rs`)

`rust_main` declares `_stext` / `_etext` as zero-length static symbols and uses only `.as_ptr()` to build the instruction address range for the backtrace.

Invariants:

- The linker script must define both symbols, and `_stext <= _etext`.
- The code reads only the symbol addresses and never dereferences the zero-length objects.
- The backtrace frame-pointer range runs from `PAGE_OFFSET` to `usize::MAX`, covering the primary core's task stack and the secondary cores' linearly mapped boot stacks.

### `.init_array` traversal (`src/init_setup.rs`)

`init_cb()` interprets `[__init_array_start, __init_array_end)` as a slice of `extern "C" fn()` and calls each entry in turn.

Invariants:

- The start and end addresses exported by the linker script form a single contiguous `.init_array` range.
- The range is aligned to the function pointer size, and its length is an integer multiple of the function pointer size.
- Each slot is generated by trusted code such as `#[register_init]` and points to a valid `extern "C" fn()`.
- The calls happen in the late-init bootstrap thread: the scheduler and local IRQs are already enabled, but PID 1 has not yet been spawned; callbacks must not depend on the user-space init already existing.

Risk: if this table is incorrectly linked or corrupted in memory, `init_cb` may jump to an arbitrary address, which makes it a high-risk boot-time boundary.

### Secondary core boot stacks (`src/mp.rs`)

`SecondaryBootStacks` uses `UnsafeCell<[[u8; TASK_STACK_SIZE]; NR_CPUS - 1]>` to hold the secondary cores' boot stacks, `stack_top()` computes each AP's stack top address through a raw pointer, and `Sync` is implemented by hand for the wrapper type.

Invariants:

- `start_secondary_cpus()` allocates `secondary_cpu_index` serially, and takes a stack only when `secondary_cpu_index < NR_CPUS - 1`.
- The backing array lives in `.bss.stack`, and its lifetime covers the entire kernel run.
- Each secondary core receives only the one-past-end stack top address of its own separate slot, and the module does not expose references to the inner array.
- The `SCHED_READY_CPUS` handshake ensures that before the primary core starts the next AP, the current AP has entered the runtime and registered its run queue, so cross-core spawns do not hit an unregistered RQ when the primary core enters `init_drivers`.

### Timer handler (`src/lib.rs`)

The timer IRQ handler registered in `init_interrupt()` contains no raw per-CPU access. It only calls `ktask::on_timer_fire()`, which owns timer re-arming.

### Raw memory access in kernel-mode unit tests (`src/mp.rs`, `#[cfg(all(feature = "smp", unittest))]`)

The SMP TLB shootdown unit test uses volatile reads and writes to verify whether remote CPUs see the page-table update.

Invariants:

- The test page is allocated exclusively by the test and freed in the cleanup phase.
- The test virtual address is mapped to the corresponding physical page before any read or write.
- The related unsafe code is built only under the `unittest` configuration and is not part of the production boot path.

### panic handler (`src/lang_items.rs`)

The panic handler itself is safe Rust, but capturing a backtrace on a corrupted stack or corrupted page tables may fail again. The current policy is to print the panic message and backtrace, and then call `khal::power::platform_power_off()` to terminate the system. The panic path deliberately bypasses the SMP stop hook of `khal::power::power_off()`: the hook halts CPUs through IPI, while the panicking CPU may be holding a lock that other CPUs spin-wait on, so going through the hook would deadlock.

## Memory safety invariants

- `BootInfo` must be readable during `rust_main(arg)`, and the FREE/RSVD/device regions described by its memory table must not contradict one another.
- The in-page offsets of the PA and VA of the boot console fixed mapping must be consistent; otherwise `register_boot_console_runtime_region()` panics immediately, to avoid registering a wrong mapping.
- The `.init_array` boundaries must lie within the mapped kernel image and must not be overwritten by writable data.
- The secondary core boot stack array must cover at least the number of APs actually started; present CPUs beyond `NR_CPUS - 1` are not started by `kruntime`.
- PID-less late-init threads and ordinary kernel workers do not enter the root PID allocator; PID 1 can only be consumed and asserted by the init spawn in `SystemInitEntry::enter()`.

## Thread safety

- `INITED_CPUS: AtomicUsize` is private boot-lifetime state. Only the primary
  publishes the first Release increment, releasing AP setup. Each AP publishes
  once after its local service initialization. The primary's final Acquire load
  observes the Release RMW sequence before entering device discovery or drivers.
  The counter describes CPU services, not completed filesystem/network startup.
- `SCHED_READY_CPUS` only expresses that an AP has registered its run queue (after entering the runtime, it goes on to finish memory management and scheduler initialization); it does not mean AP initialization is complete. It is kept separate from `INITED_CPUS` to avoid conflating "schedulable across cores" / "the next AP may be started" with "global runtime ready".
- `LogContextIf::cpu_id()` and `task_id()` avoid reading the possibly not-yet-ready current task / CPU-local runtime state before `is_init_ok()`; in non-SMP builds the CPU ID is fixed to 0.
- APs retain the startup preemption guard through fixed-notification enable,
  softirq/worker setup, IRQ enable, watchdog/PMU setup, and completion publication.
  They must not wait for primary driver initialization or local runnable tasks
  during this phase. The primary would otherwise wait for an AP that cannot finish.

## Threat analysis

| ID | Threat description | Impact level | Trigger condition | Mitigation |
|------|----------|----------|----------|----------|
| T-01 | An `.init_array` slot is not a valid function pointer, and `init_cb` jumps to an illegal address | High | Linker script error, malicious/corrupted object file, memory corruption | Trust only entries generated by the registration macro; the linker script retains and delimits `.init_array`; calls are centralized in the boot phase |
| T-02 | The `BootInfo` memory table or CPU topology is wrong | High | Boot-layer bug, abnormal firmware table, virtualization environment error | The runtime view is established by `khal::firmware` / `khal::mem` / `kcpu_id_map`; later initialization consumes only that view |
| T-03 | Shared services or drivers use AP services before preparation completes | Medium | Publishing `INITED_CPUS` before local service setup, or starting RCU/drivers before the barrier | Mitigated by separate run-queue registration and service-completion barriers and Release/Acquire publication. Unittest holds AP publication until the primary enters its wait and observes completion at the pre-RCU initialization boundary. |
| T-04 | A secondary core uses a wrong or overlapping boot stack | High | AP boot loop goes out of bounds, concurrent boot reuses a slot | `secondary_cpu_index < NR_CPUS - 1`; serial `boot_ap` + `SCHED_READY_CPUS` handshake; a separate slot per AP |
| T-05 | PID 1 is consumed early | Medium | A late-init thread or background worker obtains a Linux-visible identity; `SystemInitEntry` has not created init first | boot, idle, late-init and ordinary kernel workers use a PID-less identity; the init creation path asserts that the root PID is 1 |
| T-06 | IRQs are enabled before handlers are registered | High | The order in `init_interrupt` is broken, the platform enables IRQs early | The primary core first installs the softirq hardirq-exit runner, then registers the timer/IPI/PMU handlers, and finally enables local IRQs; secondary cores enable IRQs only after initialization completes |
| T-09 | The panic handler fails again on a corrupted stack | Medium | Stack overflow, page-table corruption, wrong backtrace range | After a panic, only diagnostics are done and then the system is powered off, with no attempt at recovery |
| T-10 | Boot logs leak physical addresses and device mappings | Low | Boot logging is enabled | Limited to the kernel log; no user data is handled, but at deployment time its visibility must be restricted according to the log policy |

## Failure Mode and Effects Analysis (FMEA)

| ID | Failure Mode | Failure Cause | Local Effect | System Effect | Severity | Mitigation |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | The global allocator has no usable FREE region | Firmware memory map is wrong or the boot layer fails to report it | Allocator initialization is incomplete | Early allocation fails or panics; the system cannot boot | 1 | Fail fast during startup; rely on boot/mem subsystem validation |
| F-02 | The page offsets of the boot console MMIO VA/PA are inconsistent | `BootInfo` is filled in incorrectly | `assert_eq!` fails | Boot is aborted | 2 | Register the fixed device region only after an explicit offset check |
| F-03 | A secondary CPU does not increment `SCHED_READY_CPUS` | `boot_ap` fails, firmware does not wake the AP, or the AP traps during memspace/scheduler initialization | The primary CPU is stuck in AP bring-up | late init cannot continue | 2 | `start_secondary_cpus()` returns the `boot_ap` error; a startup hang relies on platform logs/debugging |
| F-04 | A CPU never publishes service completion | AP setup hangs or waits for primary driver startup | The CPU-service barrier does not complete | Driver initialization and system init cannot start | 2 | No waits on drivers or local runnable tasks during AP setup; boot logs and optional watchdog/external reset. There is no local timeout. |
| F-05 | A `register_init` callback panics | Bug in a subsystem init | The `.init_array` traversal is interrupted | Subsequent inits do not run; the system powers off after the panic | 2 | Keep callbacks short and non-blocking; audit the panic path of any new callback |
| F-06 | The `SystemInitEntry` provider is missing or duplicated | `entry` provides no implementation or multiple implementations conflict | Link failure | The image cannot be generated | 2 | `kiface` exactly-one provider is detected at build time |
| F-07 | PID 1 assertion fails | A Linux-visible task consumes the root PID before init | `assert_eq!` panic | Boot is aborted | 2 | boot, idle, late-init and ordinary kernel workers use a PID-less identity |
| F-08 | The region log summary array overflows | More than 64 distinct region name/flags | `expect("too many region log summaries")` panic | Failure in the boot logging phase | 3 | `MAX_REGION_LOG_SUMMARIES = 64`; unusual platforms need a larger limit or an adjusted summarization strategy |
| F-09 | The timer handler fires again immediately or the deadline goes backward | Abnormal time source or broken deadline update logic | Ticks are too dense and scheduling is abnormal | Performance degradation or hang | 2 | The deadline takes the larger of the current record and `now + interval`, then writes the next tick |
| F-10 | NMI mechanism probing or per-CPU enabling fails | Platform lacks FEAT_NMI / GIC version too old / wrong initialization order | This CPU has no NMI or hardlockup is disabled | The system loses hard lockup detection (soft lockup detection remains) | 3 | Boot log + explicit degradation via the return values of `mode()` / `enable_periodic_nmi` |

## Fault Management

- **Fail fast on startup errors**: critical paths use `assert!`, `expect` or `panic!` to avoid running degraded in a kernel that is not fully initialized.
- **Power off after panic**: the panic handler prints the panic message and backtrace, then calls the bare platform endpoint `khal::power::platform_power_off()` (it does not go through the SMP stop hook, to avoid an IPI stop deadlock caused by a panic while holding a lock); this crate does not attempt recovery.
- **No local timeout on an SMP hang**: the waits on `SCHED_READY_CPUS` / `INITED_CPUS` use spinning; if an AP or the primary CPU's late init hangs, a watchdog feature, platform logs or an external reset must intervene.
- **Disabling a feature is a configuration choice**: when `fs`, `net`, `watchdog`, `pmu` and the like are not enabled, the corresponding initialization is skipped and is not treated as a runtime fault.

## Privacy Analysis

`kruntime` does not handle user data, file contents or network payloads. It may print the following in boot logs:

- `BootInfo` addresses;
- CPU IDs;
- Physical memory regions and the sources of reserved regions;
- PA/VA ranges of runtime device/iomap;
- The task owner key after init completes.

This information is valuable for kernel debugging, but in product configurations facing untrusted operators its visibility should be restricted according to the system log policy.

## Known Limitations

1. **`.init_array` has no per-entry isolation**: a panic in one callback interrupts the subsequent callbacks; there is no per-callback recovery.
2. **SMP waits have no timeout**: a failure in AP startup or the init barrier causes a spin wait.
3. **System init is triggered only by the primary CPU's late-init thread**: after secondary CPU initialization completes, they only enter idle and do not execute `SystemInitEntry`.
4. **PID 1 semantics depend on the `SystemInitEntry` contract**: `enter()` must create, publish and activate the root PID 1 init.
5. **The boot log summary is heuristic**: only regions whose names begin with `uefi ` and whose count reaches the threshold are summarized, which may hide the details of an individual region.

## Audit Checklist

When modifying `kruntime`, verify the following:

- [ ] When adding or moving an `unsafe` block, also document the real invariants, the calling context and the effect of failure.
- [ ] When changing the order in `rust_main`, re-check the dependencies among allocator, memspace, driver resource provider, logger, scheduler and IRQ.
- [ ] When changing the order in `late_init_main`, re-check the ordering of `.init_array`, the SMP ready barrier, the vsock/TIPC bridge and `SystemInitEntry`.
- [ ] When modifying `INITED_CPUS` / `SCHED_READY_CPUS`, distinguish "the AP can be scheduled across cores (run queue registered)" from "all CPU runtimes are ready", and check the Release/Acquire pairing.
- [ ] When adding an early/late kernel thread, confirm whether it should be a PID-less `Internal` thread so that it cannot consume PID 1 first.
- [ ] When modifying the `SystemInitEntry` provider, confirm that it returns after spawning and activating PID 1.
- [ ] When adding a `register_init` callback, confirm that it does not depend on user-space init, avoids long blocking, and accepts that a panic aborts startup.
- [ ] When modifying the timer/IPI/PMU handlers, confirm that handler registration precedes IRQ enable.
- [ ] Do not reintroduce a DMA page table adaptation layer in the runtime; the related checks and locks are the responsibility of the MM API provider of `memspace`.
- [ ] When modifying boot logs, assess whether they newly leak physical addresses, device mappings or task identities.

## ARM64 counter recovery dispatch

With `arm-timer-resume-fixup`, the timer IRQ handler dispatches pending counter-recovery notifications before processing task timers. Clock reads only repair shared counter state and rearm CPU-local registers; IPI delivery is deferred until caller-held clock-read locks have been released. The dispatch runs without the counter lock, does not sleep, and requires initialized interrupt routing. It does not reconstruct elapsed host sleep time.

DMA mapping attributes no longer pass through a runtime adapter: kiface-mm-api owns
DmaMappingMode and DmaMappingIf; memspace directly implements the transition.
## IRQ Management Exclusion

Runtime no longer implements an IRQ management-lock adapter. KIRQ checks context
and holds a `kmutex` guard directly; `ktask` supplies the shared mutex's scheduling
operations. The runtime lifecycle test validates actual task waits with a
test-owned in-flight operation, without enabling a hardware source or pretending
to exercise controller completion. Startup ordering remains runtime's duty.

Mandatory timer/IPI descriptors are prepared before handler registration while
ordinary CPU IRQ delivery remains masked. A preparation error is fatal before
activation; no rollback of a partially prepared fixed set is attempted because
boot cannot proceed. These are platform-lifetime identity descriptors, not
device-local hwirqs translated by guesswork. AP activation reuses them. Startup
registration requires resolved PerCpu flow and disabled local slots before
publishing either action. Runtime uses `try_register_percpu_startup` and never
falls back to ordinary registration on failure. Each CPU explicitly activates
its sources only after handler publication. Startup tests observe descriptor
identities, PerCpu classification and phase ordering; they do not establish source
quiescence or memory reclamation safety for dynamic device mappings.

## System power management boundary

System PM admission, platform trust and rollback are owned by `kpm`, not the
runtime startup layer. See `../../kpm/docs/security.md`. Runtime initialization
still establishes the scheduler, time and IRQ prerequisites required by PM.

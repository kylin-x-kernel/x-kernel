# kruntime — 设计文档

## IRQ Startup Ownership

Runtime owns the cross-subsystem ordering: install current, publish foundation
handlers, activate local sources, prepare IRQ-exit services, then enable CPU IRQ
delivery. The AP runqueue-ready and service-ready rendezvous serve different
consumers and remain separate. Neither a controller driver nor kirq can infer
that scheduler, clockevent and worker prerequisites have all completed.

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
After GP and worker startup, the boot task invokes `khal::console::prepare_input`
before device discovery can construct N_TTY. The console provider owns optional
input setup and polled fallback; runtime only orders this phase. Validation and
coverage limits for controller startup are recorded in
[IRQ startup validation](irq-startup-validation.md).

## RCU Host Adapters

The execution-context adapter reports the common IRQ core's context facts,
including the distinction between BH exclusion and serving a handler. It
preserves NMI-first queries and existing snapshot protection, without deciding
RCU scope admission, watching or cleanup eligibility.
It also supplies CPU identity and exception presence. The scoped exception
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

The timer adapter owns the concrete ktask Timer and lends a one-shot capability
on its stack. It translates monotonic nanoseconds to the timer deadline type,
preserves owner routing across migration and synchronously drains delivery on
return. The expiry function has no captured allocation; RCU's permanent service
receives it and publishes demand. Intervals, deadline retention after early
wakes, assistance and GP completion remain krcu policy. Service readiness is
published from inside the adapter body, after timer preparation.

The same integration module lends current task reader storage to krcu through
`RcuTaskIf`. Its stack-local CurrentTask borrow spans the synchronous body,
without allocation or an owner-count update. It supplies no scope-admission or
cleanup policy; those remain in krcu. IRQ/NMI use the same storage adapter without
locks or changes to IRQ/preemption state.

Task preemption controls delegate directly to ktask: querying its actual count,
setting a request, running a normal checkpoint and deferring checks around one
synchronous body. Runtime adds no scheduling policy or credit of its own. RCU
owns cleanup decisions; ktask owns count balancing and switch eligibility.

The synchronous task-wait adapter owns a `ktask::completion::CompletionWait`
on its stack and lends infallible `wait_for_completion` operations. Preparation
validates sleepability before calling the RCU body; preparation and waiting do
not allocate or use poll registrations. RCU keeps callback completion and
retirement ownership, including its existing callback/drain Arc allocations.
The separate service-wait adapter retains `PreparedTaskWait` waker cloning and
park operations for startup acknowledgment and GP progress. Wake hints merely
allow those callers to check their predicates again. Neither adapter returns
a preparation error after invoking its body.

The drain adapter holds one permanent sleepable mutex across the supplied body,
including its completion wait. Lazy initialization constructs the mutex on the
first admitted drain call, before any marker is installed. The mutex payload
contains no RCU state: markers, counters and completion checks belong to krcu.

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
`KFEAT_RCU_BENCH` selects the independent callback/synchronous-wait runner through the
same handoff. Kconfig excludes both stress runners while it is selected.

## 定位

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
holds the host drain gate and verifies that blocked drain callers install nothing.
Isolated startup, deadline and completion policy tests remain in krcu and use
its host contracts without depending on concrete task or IRQ implementations.

Startup distinguishes a published run queue that accepts tasks from a CPU whose
services are ready to execute them. After scheduler initialization, each AP holds
NoPreempt before publishing SCHED_READY_CPUS, through fixed-notification drain,
ksoftirqd creation, IRQ enable and optional watchdog/PMU initialization. It releases
the guard before run_idle. Requests remain pending rather than being discarded.
IRQ masking alone is insufficient because internal lock release can check preemption.

主核在 scheduler 初始化返回后，同样保护 IRQ 服务与 late-init 任务的创建和激活，
完成后释放 guard，再进入普通调度。late_init_main 的驱动/文件系统/网络阶段可能
等待 I/O，不处于该保护范围。受保护启动区域不得显式 yield、阻塞或等待本 CPU
尚未执行的任务；后续新增初始化步骤必须维护这个约束。

单测启动钩子在所有 AP 的 run queue 就绪、主核尚未发布 INITED_CPUS 时逐核
投递任务及重调度请求，任务检查 AP 服务准备完成标记。结果保留至测试执行，
不依赖忙等时长；测试任务退出，无运行期 hook 或栈借用遗留。

Runtime enables fixed notifications after `kipi::init` on the primary, then
releases AP service initialization. Each AP enables notifications before creating
its local services and publishing completion. The notification entry enables the
line, publishes ENABLED, and drains early requests. `SCHED_READY_CPUS` still means
run-queue registration, not IRQ readiness; static reason bits retain early requests.

`kruntime` 是 x-kernel 的**运行时编排层**：在平台引导代码将控制权交给内核后，负责主核/从核的初始化顺序、子系统装配，以及跳转到系统初始化入口（`entry` crate 提供的 `SystemInitEntry`）。它不实现具体设备驱动或 syscall，而是把 `khal`、`memspace`、`ktask`、`kdriver` 等按固定阶段串联起来。

目标读者：需要理解启动链、SMP 屏障、或在此 crate 增加初始化钩子的内核开发者。

## 背景

x-kernel 将**引导**（`kernel-boot`、平台 `platconfig`）与**运行时**（内存、调度、驱动、用户态 init）分离。引导层通过 `BootInfo` 和 `kiface` 单实现入口把主核/从核控制权交给运行时；`kruntime` 提供这些入口的实现（`rust_main` / `rust_main_secondary`）。

启动拓扑遵循 FreeBSD `proc0` 模型：boot/current task 是调度器内部任务（`Internal` 身份，不进入普通 PID allocator）。`rust_main` 激活一个 PID-less 的 late-init 内核线程跑完所有 late init（drivers/fs/net/SMP 等），等待全部 CPU 就绪后调用 `SystemInitEntry::enter()`。`entry` **spawn 出一个全新的 PID 1 用户任务**（init），随后 late-init 线程退出。init 不是"原地变身"——它是被 spawn 的新任务，走与 fork 相同的 `new_user` + `publish_user_task().commit()` 标准路径。

关键设计：late-init 线程以及 late init 期间 spawn 的所有后台 worker（网络 poller、TTY、watchdog 等）都是 PID-less 的 `Internal` 内核线程（对应 FreeBSD `kthread_add`），不进入普通 PID 空间。因此 init 的分配是 root namespace 的第一笔普通分配，PID 1 是唯一的启动期固定 PID。

应用逻辑（例如 `entry::runtime::init_runtime`）放在 `entry` crate；初始用户进程组装则由 `posix-process::spawn_init_process` 承接，避免 `kruntime` 反向依赖高层服务。

## 范围

涉及的源文件：

```
core/kruntime/
├── src/
│   ├── lib.rs              # 主核 rust_main、日志适配、中断/分配器、内存区日志
│   ├── mp.rs               # SMP 从核启动、TLB 单测
│   ├── init_setup.rs       # .init_array 构造函数表遍历
│   ├── dma_integration.rs  # kdma::DmaPageTableIf 实现
│   └── lang_items.rs       # #[panic_handler]
├── Cargo.toml
└── docs/
    ├── design.md
    └── security.md
```

## 架构

```
kernel-boot (汇编 / MMU / BootInfo)
        │
        │  PrimaryKernelEntry::enter(boot_info_ptr)
        ▼
┌───────────────────────────────────────────────────────────────┐
│  kruntime::rust_main (主核, boot task)                         │
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
│  kruntime::rust_main_secondary (从核)                          │
│    percpu / trap / memspace 从核 / 调度器从核 / IPI            │
│    wait for primary -> local services -> publish -> idle      │
└───────────────────────────────────────────────────────────────┘

侧向接线（链接期，非启动顺序）：
  dma_integration  ──provide──►  kdma::DmaPageTableIf  ──►  memspace
  lib.rs            ──provide──►  klogger::LoggerAdapter
  lib.rs            ──provide──►  kernel_boot::PrimaryKernelEntry
  mp.rs             ──provide──►  kernel_boot::SecondaryKernelEntry
```

| 组件 | 职责 |
|------|------|
| `rust_main` | 主核唯一完整初始化路径；由 `PrimaryKernelEntry` provider 进入 |
| `rust_main_secondary` | 从核初始化；由 `SecondaryKernelEntry` provider 进入；最终 `run_idle` |
| `mp::start_secondary_cpus` | 为 AP 准备栈、调用 `boot_ap`、等待 AP 注册其 run queue（跨核 spawn 安全前提） |
| `init_setup` | 执行链接段 `.init_array` 中的 `register_init` 回调 |
| `dma_integration` | 将 DMA 页表属性更新委托给 `memspace::kernel_layout` |
| `lang_items` | Panic 时打印 backtrace 并调用裸平台终点 `khal::power::platform_power_off`（绕过 SMP stop 钩子，避免 panic 持锁时 IPI 停机死锁） |

## 启动流程（主核）

`rust_main` 阶段划分如下（与代码顺序一致）：

```
BootInfo*
    → firmware::init / percpu::init_primary / init_trap
    → mem::init
    → init_allocator (kalloc 全局堆)
    → register_boot_console_runtime_region (可选 MMIO UART)
    → memspace::init_memory_management
    → early_driver_init
    → klogger + backtrace (链接符号 _stext/_etext)
        (日志等级：优先取启动命令行 `loglevel=` / `quiet` / `debug`，否则 Kconfig 默认)
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
    → [vsock_tipc_bridge] start_vsock_bridge (依赖跨 CPU 调度，必须在屏障后)
    → SystemInitEntry::enter()
        (entry; spawn 出全新 PID 1 用户任务 init，返回)
    → late-init thread exits (task_entry 自动调 ktask::exit)

init (PID 1, 全新 spawn 的用户任务):
    → 调度器 switch-in 时自动激活用户页表 (switch_page_table_root)
    → run_user_thread_loop → 进入用户态
```

### PID 1 启动约定

`ktask::init_scheduler()` 创建的 boot/current task 与 idle task 都是 scheduler 内部身份（`Internal`/`Idle`），不调用 `kidentity::allocate_root_pid_handle()`。late-init 线程也是 `Internal`，且 late init 期间 spawn 的所有后台 worker（`ktask::spawn` 走 `new_pidless_kthread`）同样是 PID-less 的 `Internal`。因此 `SystemInitEntry::enter()` 创建 init 时，root namespace 的第一笔普通 PID 分配必须得到 PID 1。

这个顺序把 root namespace 的早期 PID 语义固定为：

```text
PID 0   = boot/idle/swapper 类内部任务（不经普通 allocator）
PID 1   = init 用户进程（late-init 线程 spawn 的全新用户任务）
PID >=2 = 后续 fork 出的用户进程或显式 Linux-visible task；普通内核 worker 不占号
```

init 不再经历"原地变身"：late-init 线程通过 `SystemInitEntry::enter()` → `posix-process::spawn_init_process` 构造一个全新的 `User` 身份任务，runtime 在 `new_user` 构造时一次性就绪（`UserRuntimeSlot::ready`），经 `publish_user_task().commit()` 发布并激活，页表由调度器在首次 switch-in 时通过 `switch_page_table_root` 自动写入。没有 `install_user_runtime`、空槽状态机或 `activate_current_user_page_table` 这类"事后补装"机制。

### 全局就绪屏障 `is_init_ok`

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

### NMI 初始化时序

NMI 机制由平台探测并在每 CPU 使能，`kruntime` 负责在正确的初始化阶段调用
`khal::nmi` 接口：

- `early_driver_init`（GIC 初始化完成后）→ `khal::nmi::early_init()`：探测 GIC
  版本与 FEAT_NMI，写入运行时机制（`Pseudo` / `Hardware` / `None`），并打印
  `NMI mode: ...` 启动日志。
- 主核 `prepare_interrupts` 与每个从核 `prepare_local_interrupts` → `khal::nmi::late_init()`：每 CPU
  使能。hardware 模式在此开启 `SCTLR_EL1.NMI` / ALLINT；pseudo 与降级（`None`）
  无需 per-CPU 寄存器操作。
- late-init 线程中的 `[watchdog] init_primary`（以及从核 idle 前的
  `init_secondary`）调用 `init_nmi_watchdog`：`mode() == None` 时打印
  "hard lockup detection disabled" 并跳过武装；否则经
  `khal::nmi::enable_periodic_nmi` 用 PMU 周期源武装硬锁检测。
- PMU 溢出分发由 `pmu` feature init 独立注册（normal IRQ 或 NMI fallback），与
  watchdog 解耦，因此降级平台上 perf 事件仍可用。

## 启动流程（从核）

```
SecondaryKernelEntry::enter(logical_cpu_id)
    → percpu::init_secondary, init_trap
    → memspace::init_memory_management_secondary
    → prepare_local_interrupts, init_scheduler_secondary
    → SCHED_READY_CPUS += 1  (供 start_secondary_cpus 等待; RQ 已注册, 跨核 spawn 安全)
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
    → run_idle()  (永不返回)
```

`start_secondary_cpus` waits for each AP's `SCHED_READY_CPUS` publication after
scheduler initialization before starting the next AP. This permits cross-CPU
task placement but does not permit assuming that local services are ready.
The primary subsequently completes `init_cpu_services` before entering drivers.

## `.init_array` 与 `register_init`

`init_setup.rs` 维护链接器段 `.init_array`：

- `_SECTION_PLACE_HOLDER`：保证空镜像也有 `__init_array_start` / `__init_array_end`。
- `init_cb()`：late-init 在 driver probe 和 entropy 初始化后、filesystem namespace 构造前，
  按指针步进调用段内每个 `extern "C" fn()`。

各子系统可通过 `util/macros` 的 `#[register_init]` 向该段注册早期 init，无需修改 `kruntime` 源码列表。

## 已知限制：loongarch64 QEMU 直启没有 ACPI 表

`make justrun`（loongarch64）用 `-kernel` 直启，QEMU 的
`loongarch_direct_kernel_boot` 伪造的 EFI system table 只含
memmap/initrd/DTB 三个 configuration table 条目；ACPI 表
（`virt_acpi_setup`）只通过 fw_cfg 文件（`etc/acpi/tables`、
`etc/acpi/rsdp`）暴露，**从不写入 guest RAM**——它们只有在 UEFI 固件
启动（固件读 fw_cfg 并把表拷进 RAM、在 EFI config table 挂 RSDP 条目）
时才可达。因此直启下 `rsdp=0`、表解析休眠属预期行为而非缺陷（Linux
在同样启动方式下也拿不到 ACPI）。断电由 DTB 的 `syscon-poweroff`
声明兜底（直启虽无 ACPI 表，但 QEMU 的 DTB 声明了 GED
sleep-control 寄存器地址与 S5 写值）。x86-64（SeaBIOS）不受影响。

## `kiface` 接线

| 接口 | 实现位置 | 目的 |
|------|----------|------|
| `kdma::DmaPageTableIf` | `dma_integration.rs` | 打破 `kdma` ↔ `memspace` 循环依赖 |
| `klogger::LoggerAdapter` | `lib.rs` | 日志输出到 `khal::console`，附带 CPU/任务 ID |
| `kernel_boot::PrimaryKernelEntry` | `lib.rs` | 主核从 boot 层进入 `rust_main` |
| `kernel_boot::SecondaryKernelEntry` | `mp.rs` | 从核从 boot 层进入 `rust_main_secondary` |
| `kruntime::SystemInitEntry` | `entry/src/main.rs` | runtime 就绪后进入系统级 init 策略层 |
均为链接期 exactly-one 单实现，非运行时注册。

## Cargo Features

| Feature | 作用 |
|---------|------|
| `smp` | 从核启动、`rust_main_secondary`；跨模块 SMP 配置由 `kfeat/smp` 统一启用 |
| `ipi` | 依赖 `kipi`；IPI 中断处理 |
| `fs` / `fs9p` / `net` / `vsock` / `display` / `input` | 驱动与子系统初始化（经 `kdriver`）；具体 filesystem feature 只负责链接其自注册实现 |
| `rtc` | 启动时打印墙钟时间 |
| `watchdog` / `watchdog_hardlockup` | 看门狗主/从核初始化；`watchdog_hardlockup` 额外启用 NMI 驱动的硬锁检测（`watchdog/nmi`） |
| `pmu` | PMU 溢出中断；`kruntime` 在 `pmu` feature 下独立注册溢出分发（`khal::pmu::register_overflow_dispatch`），perf 与 watchdog 解耦；配合 `watchdog_hardlockup` 时 PMU 线可被提升为周期 NMI 源 |
| `arm-timer-resume-fixup` | 在每次读时检查 AArch64 计数器回退，并在定时器中断中发送延后的恢复通知 |
| `rootfs-secondary-block` | 将第二个块设备作为根文件系统后端 |

默认全部关闭；由 `entry` / `kfeat` 的 defconfig 打开。

## 设计决策

### 为何用 `SystemInitEntry` 而非直接依赖 `entry`

`kruntime` 被 `entry` 依赖。若 `kruntime` 再依赖 `entry` 会形成环。`SystemInitEntry` 把 handoff 变成一个 `kiface` 单实现接口：`kruntime` 拥有调用契约，`entry` 提供策略实现，同时避免依赖裸符号名和 `extern "C"` 调用。

### 为何主核才跑 `init_cb`

`.init_array` 回调假定中断子系统已注册、且尚未进入多任务应用阶段；放在 `init_interrupt` 之后、调用 `SystemInitEntry::enter()` 之前，与 C 运行时 constructor 时机相近，但由内核显式控制调用点。

### 为何 DMA 接线放在独立模块

保持 `lib.rs` 聚焦启动编排；`dma_integration` 仅一行业务逻辑，便于审查与替换实现。

### 日志适配放在 `lib.rs`

`LoggerAdapter` 需在 `klogger::init_klogger` 之前通过 `kiface` provider 链接生效；与 `rust_main` 同文件可避免初始化顺序问题。

## 与 QEMU / 真机的边界

`kruntime` 本身**平台无关**：`BootInfo` 内容由 `kernel-boot` + 平台 crate 填充。QEMU virt 与板级（如 raspi）差异在引导层和平台准备/激活 hook，进入 `rust_main` 后的阶段表相同。

## 冗余与过载

- **冗余设计**：无（单路径主核 init）。
- **过载控制**：不在此 crate；调度与内存压力由 `ktask` / `kalloc` 负责。

## ARM64 counter recovery dispatch

With `arm-timer-resume-fixup`, the timer IRQ handler dispatches pending counter-recovery notifications before processing task timers. Clock reads only repair shared counter state and rearm CPU-local registers; IPI delivery is deferred until caller-held clock-read locks have been released. The dispatch runs without the counter lock, does not sleep, and requires initialized interrupt routing. It does not reconstruct elapsed host sleep time.

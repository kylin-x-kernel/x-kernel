# khal — Design

This document covers the IRQ trap-entry adapter, crate test layout, and page-table
storage adapter. Other HAL subsystems retain their own interface contracts.

The NMI facade also exposes the platform's `shutdown_periodic_nmi`: a local,
fallible reusable-source shutdown, distinct from terminal `quiesce_nmi`. HAL
does not broadcast it, free KIRQ handlers or interpret queue submission as
completion. The source owner coordinates those steps.

The PMU facade forwards `shutdown_overflow_irq` and `restore_overflow_irq` to
that source owner through `PmuInterruptIf`. These task-context operations own
global PMU retirement/republication in the platform provider, not HAL. The local
NMI operation and terminal stop remain separate APIs with different lifetimes.

## 定位

`khal::irq` 是 IRQ trap-entry adapter。通用 IRQ core
位于 `kirq`；外部调用方不应再通过 `khal::irq::*` 使用 generic IRQ API。

本模块现在只拥有 trap-entry 相关职责：

- 注册 `IRQ` / `NMI` trap handler；
- 在进入 `kirq` 前建立 `NoPreempt` 执行约束。

## 范围

```text
arch/khal/src/
├── irq/
│   ├── mod.rs       # adapter helper exports only
│   ├── manager.rs   # trap handlers
│   └── nmi.rs       # khal::irq::configure_nmi（每 CPU 中断线提升，委托平台 NmiMechanismIf）
└── lib.rs           # khal::nmi / khal::pmu 机制中立 facade（re-export kplat）
```

Generic IRQ state, descriptors, dispatch, Line-owned NMI actions, lifecycle
hooks and `IntrManagerIf` belong to `arch/kirq`. `khal::nmi` is a
mechanism-neutral facade; mechanism details belong to `kplat::nm_irq` and its
platform implementations.

## 架构

```text
kcpu trap dispatch
    |
    `-- khal::irq
            |-- #[register_trap_handler(IRQ)]
            |-- #[register_trap_handler(NMI)]
            |-- NoPreempt guard
            `-- kirq::{handle_irq, handle_nmi}
```

普通 IRQ API 调用路径应直接进入 `kirq`，不经过 `khal::irq` facade。

NMI 机制 facade：

```text
watchdog / perf 消费者
    |
    `-- khal::nmi（re-export kplat::nm_irq）
            |-- early_init / late_init   # 机制探测 + 每 CPU 使能
            |-- mode / info              # 运行时机制查询（Pseudo / Hardware / None）
            |-- enable_periodic_nmi      # 周期 NMI 源（当前为 PMU）
            |-- quiesce_nmi               # 终止路径停靠本地周期源
            `-- khal::irq::configure_nmi # 每 CPU 提升中断线
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

## 调用约束 / 执行上下文

- `irq_handler()` 与 `nmi_handler()` 只能由架构 trap dispatch 调用。
- 两个 handler 都先创建 `kspin::NoPreempt`，再调用 `kirq`。
- `irq_handler()` 调用 `kirq::handle_irq()`；hardirq lifecycle enter/exit 和
  IRQ completion 顺序由 `kirq` 负责。
- `nmi_handler()` 调用 `kirq::handle_nmi()`；NMI raw-hwirq dispatch 语义由
  `kirq` 负责。
- `khal::irq` stores neither IRQ registry state nor NMI handler ownership;
  both remain in the single `kirq` core.
- `khal::nmi::early_init()`：GIC 初始化后、任何 NMI 使能前调用一次（平台
  early_driver_init），探测机制并写入运行时 `NmiMode`。
- `khal::nmi::late_init()`：主核 `prepare_interrupts` 与每个从核 `prepare_local_interrupts` 各调用
  一次（每 CPU）；hardware 模式在此开启 `SCTLR_EL1.NMI` / ALLINT 使能。
- `khal::nmi::enable_periodic_nmi(period_ns, cb)`：每 CPU 调用；返回 `false`
  表示本 CPU 无法武装（机制不可用或资源冲突），消费者应显式降级。
- `khal::quiesce_nmi()`：仅供终止停机路径调用；以 `NoPreempt` 钉住当前 CPU，
  停掉已武装的本地周期源但保留回调和 IRQ 配置。没有 `nmi_pmu` source 或本地
  source 未初始化时为 no-op，调用后不得重新武装。
- `khal::irq::configure_nmi(hwirq)` delegates CPU-local preparation through the
  platform to KIRQ. Initial preparation requires a registered disabled source;
  repeated preparation rechecks admission without rewriting live attributes.
  Shared sources are unsupported. False means rejected/failed preparation,
  including an unavailable mechanism or a closing Line; it does not enable IRQs.
- `khal::nmi::mode()`：返回 `NmiMode::{Pseudo, Hardware, None}`，供消费者
  （如 watchdog）判断是否启用依赖 NMI 的功能。

## 并发模型

`khal::irq` adapter 只有短生命周期的 `NoPreempt` guard，不持有全局 IRQ state。
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

## 设计决策

### 为什么保留 `khal::irq`

trap handler registration 依赖 `kcpu::excp::register_trap_handler`，并且进入
`kirq` 前需要由 HAL adapter 建立 `NoPreempt` 执行约束。因此 `khal::irq`
继续存在，但不再承载 generic IRQ API 或 x86 MSI-X vector helper。

### 为什么 x86 MSI-X helper 不在 `khal::irq`

MSI-X vector allocation、APIC destination selection 和 MSI message composition
属于 IRQ core/backend 边界。当前由 `kirq::MsiBackendIf` 定义 backend contract，
由 `drivers/platform/x86-apic` 实现；`khal::irq` 不再暴露 APIC id 或裸 CPU vector。

### 为什么 `IPI_IRQ` 不进入 `kirq`

`IPI_IRQ` 来自 Kconfig 生成常量。`kirq` 必须保持不依赖 `kbuild_config`，所以
IPI 调用方直接使用 `kbuild_config::IPI_IRQ` 并调用 `kirq::notify_cpu()`。

## Crate-level guest test layout

Guest tests live in the crate-root `ut-tests/` directory and are mounted at
their original source modules using `unittest::path_mod_test!`:

| Source mount | Test file | Module | Cases |
|---|---|---|---|
| `src/percpu.rs` | `ut-tests/percpu.rs` | `tests_percpu` | 1 |
| `src/paging.rs` | `ut-tests/paging.rs` | `tests_paging` | 2 |
| `src/paging.rs` | `ut-tests/paging_page_refs.rs` | `tests_page_refs` | 3 |
| `src/mem/mod.rs` | `ut-tests/mem.rs` | `tests_mem` | 3 |
| `src/time/mod.rs` | `ut-tests/time.rs` | `tests_time` | 4 |
| `src/time/epoch.rs` | `ut-tests/time_epoch.rs` | `tests` | 7 |

These 20 cases require `cfg(unittest)` without additional feature, architecture,
serial or user gates. All four supported platform unittest builds select them.
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
of the generic (PA, page-count) raw pair; the page-table library remains PA-only.
The original `UsageKind` records only allocation accounting; final `PageRef::drop`
returns the page and records the release directly in kalloc. Borrowed root entries
do not own their source table pages and do not restore or release those holds.

The table owner must end hardware use and complete required translation
retirement before destroying owned storage. The existing AArch64 user-ASID
invalidation path in `PageTable64::drop` is unchanged. Reference counting retains
storage; it does not replace address-space exit, remote-CPU coordination or TLB
shootdown. This stage does not retain references for data PTEs.

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

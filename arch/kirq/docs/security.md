# kirq — 安全与可靠性分析

## Immediate Routing Boundary

This analysis includes `model/affinity.rs`, `backend/affinity.rs` and
`runtime/affinity.rs`, plus the Line and management admission checks they use.
Routing updates validate sleepable context before acquiring management exclusion;
queries reject NMI before taking any ordinary IRQ lock. Both retain the selected
Line and recheck retirement. The query returns values, not a mapping lease.

The controller is trusted to validate source identity, destination encoding and
reachability before writes. Its transaction runs bounded and non-sleeping under
Line exclusion; controller locks nest below Line and must not reenter KIRQ.
Success must preserve actual suppression or establish `KeepMasked`, and return
a nonempty effective subset of the supplied ready candidates. An invalid success
is treated as uncertain programming, not committed as a usable destination.

Post-write failure does not imply rollback, hardware masking or quiescence. The
separate routing fault blocks action admission, registration and enable; ordinary
trigger configuration cannot clear it. Disabled-source routing recovery may clear
only that fault after confirming the new route and suppression. Existing disable
depth still prevents flow tails or software replay from reopening the source
before explicit enable. Device shutdown and old-claim completion remain separate
owner/controller obligations. No new unsafe operation or per-dispatch lifetime
counter is introduced. Unsupported backends expose no routing capability.

The `cfg(unittest)` helper `tests_irq::retire_reserved_test_line` is solely for
an explicitly reserved ordinary hardware fixture. The fixture must stop its
producer, mask and drain delivery, withdraw its action and exclude other mapping
users before removing both test indexes. The helper rejects busy/enabled, MSI,
private and NMI-owned Lines and releases the returned owner outside short locks.
It uses quiescent fixture removal, not GP-delayed retirement, so the caller must
exclude all borrowed readers of the removed key, not only active actions.
It is not a production dynamic mapping-disposal API or proof of source vacancy.

## NMI Action Lifetime Boundary

`runtime/nmi/action.rs::NmiAction` owns one stable `KBox<Handler>`.
Per-CPU `AtomicPtr<Handler>` slots publish access to that immutable owner;
they are not independent action owners or reader counters. Only PerCpu NMI
registration is supported. Unbound and other flows are rejected before action
publication, so their different shutdown rules cannot bypass this lifetime.

`prepare_local_action` publishes the current CPU's slot only after successful
controller preparation, under continuous Line and CPU/IRQ exclusion.
`NmiAction::with_current` establishes its own `NoPreemptIrqSave` guard and
acquire-loads that slot. Its callback cannot return or retain the borrowed
handler. The true NMI path takes no registry, Line or management lock, clones
no owner, updates no reference count and performs no allocation or wait.

The unsafe boundary is local teardown, not an unchecked remote-stop boolean.
`teardown_percpu_nmi` requires stopped generation, excluded restart and no
outstanding claim spanning the transition. Only after backend success does
`clear_current` revoke the calling CPU's pointer under Line/CPU exclusion.
Failure retains publication and the cleanup obligation. No remote CPU may clear
another CPU's slot. A nested NMI returns before the interrupted local revoker
resumes; ordinary local revocation cannot overtake the guarded read.

For true NMI, `with_nmi_claim` owns CPU/IRQ exclusion across dispatch and original
claim completion. Its directory RCU scope retains the Line through completion.
Normal-IRQ claims for NMI-owned Lines instead complete under the already-held
Line lock without execution admission, a CPU-local owner borrow or handler
invocation. This rejection adds no diagnostics or locks. Drain takes the same
Line lock, serializing with rejected completion without proving hardware
quiescence. Directory RCU is not action reclamation.
Under management and Line exclusion, `take` refuses every nonempty local slot
before recovering the owner. Final destruction occurs outside all management,
Line and CPU/IRQ guards. No action grace period or running RCU service is
required for removal; directory retirement retains its own RCU requirements.

The PMU owner must still stop every boot-static CPU permitted to start, including
unprepared CPUs. A start can pass its gate and pause in its pinned IRQ-off
region before publishing preparation, escaping a participant-only snapshot.
Completed target calls order those old regions before restoration while Closing
remains in force. Empty enabled/prepared/publication slots do not independently
establish that ordering or clear pending hardware. Other source owners must
provide equivalent exclusion. Concurrent CPU hotplug, system stop and general
shared PMU event ownership are outside this protocol.

## CPU-local Control Boundary

NMI removal retains the handler and persistent close admission when a tracked
local slot is enabled. A failed close must not reopen enable admission or clear
another CPU's state. Only a completed local disable updates that slot. This
check does not drain true-NMI readers or replace producer shutdown. Successful
local teardown revokes publication only after claim completion; withdrawal also
checks all publication slots before recovering the owner without an action GP.
Unrelated NMI sources need not be excluded. Local preparation is retained across backend failures;
it cannot be replaced with a different backend or discarded by free.

`runtime/nmi.rs::teardown_percpu_nmi` is an unsafe boundary: its caller stops
local producer activity, prevents restart through teardown/reuse, and excludes
claims spanning the transition. Backend teardown can clear pending state.
Line/CPU exclusion protects the control transaction, not arbitrary device state
or remote handlers. The GIC backend waits for local RWP, rejects active delivery
and checks pending/attribute state; timeout never authorizes reclamation. PMU
terminal quiescence intentionally retains resources and is not this teardown.

`runtime/manager.rs::PerCpuForwarding::set_local_state` is an unsafe boundary.
The retained Arc prevents Line storage reclamation, but is not a producer or
hardware lifetime lease. Callers must keep the action registered, exclude
mapping/configuration/teardown and competing ownership changes, execute on the
owning CPU outside NMI with IRQs masked, and stop/mask the source before Host.
Guest transitions reconstruct logical activity while delivery remains masked.
The callback must not allocate, sleep, reenter KIRQ or retain a borrowed vCPU.
Backend guest-interface metadata is trusted firmware input validated for range,
alignment and non-overlap by the provider; the VMM validates register capacity
after device mapping. Missing capability must not fall back to guessed MMIO.

`runtime/state_local.rs` is protected by the retained Line lock. CPU selection,
controller programming and commit must retain IRQ/preemption exclusion; plans
cannot move to another CPU. Configuration failure closes only that CPU's action
admission. Local callbacks may overlap across CPUs, so handlers must synchronize
shared data themselves. Rejected PerCpu claims complete under the retained Line
lock without execution admission; accepted executions borrow the action through
unlocked completion under the source owner's lifetime contract, without a count
or ordinary running gate. Enabled slots block removal and flow replacement;
shutdown owners must stop and disable every participating source and wait for
every admitted dispatch to return fully before removal.

Generic regular/startup/shared registration, free, synchronous disable and
synchronize reject resolved PerCpu flow with `UnsupportedFlow` before hardware,
action or disable-depth mutation. Local nonsynchronous disable still masks the
calling CPU's source before committing its disabled slot. The Line lock and
IRQ/preemption exclusion cover apply and commit; other CPU slots remain intact.
This transition is neither producer shutdown nor a drain.

The dedicated `try_register_percpu` and `try_register_percpu_startup` contracts
are the ordinary local-action publication entrances: the resolved flow must
be PerCpu (`UnsupportedFlow` otherwise), the Line must have no action, and all
local enabled slots must be clear. Occupancy or an enabled slot rejects with
`Ok(false)` before hardware configuration or handler publication. Post-resolution
rejection may retain merged descriptor metadata; callers must not infer metadata
rollback from a failed registration. Success configures only the calling CPU
and leaves delivery disabled; activation is an
explicit operation on each participating CPU. Task registration requires
sleepable context; the startup variant retains nonwaiting startup requirements.
Existing allocation/configuration error and rejected-owner destruction rules apply.

An empty enabled set is not a quiescence certificate: previously admitted claims
may still exist and source producers may still need stopping. No PerCpu execution
counter or generic drain establishes this missing proof.
These registration checks grant neither NMI/forwarding capability nor source-owned
stop/free exclusion or runtime removal of permanent handlers.

Unsafe `try_free_percpu_irq` validates resolved flow under the retained Line lock and
management admission, before any withdrawal or controller operation. It reuses
the enabled-slot rejection and regular-action-only removal path: shared and
NMI owners cannot be removed through it. It requires sleepable task context.
Before calling, the source owner must stop every participating CPU's source,
exclude restart and new dispatch through removal, and wait for every admitted
dispatch to return, including its handler, original claim completion and tail
work. This includes software dispatch. A failed
shutdown must retain the action and resources for retry. Only that prior exclusion permits recovery of the
detached raw action storage; its separate removal closure uses no `IrqDrain`
and does not wait for PerCpu execution. An empty
enabled set is necessary, not proof that a handler which disabled itself has
returned. Mapping and device-resource lifetime remain separate responsibilities.

The PerCpu guard retains the Line and records unhandled diagnostics after the
original completion callback returns. Neither handler return nor completion
callback return proves that this tail has finished. Source-stop exclusion must
cover full dispatch return within its outer IRQ-masked interval before unsafe free.

The production caller is VMM route-preparation rollback before local activation,
serialized by its preparation mutex. Successful routes and other permanent
timer/IPI registrations have no runtime removal protocol. This boundary follows
the source-owned shutdown distinction of Linux `__free_percpu_irq`; it does not
infer shutdown from enabled bits or provide a broadcast stop operation.

## Source Binding And Dispatch Entry

Mapping, callback readiness, registration/configuration and source production
are separate lifecycle boundaries, in that order. A mapped Line alone cannot
justify starting a device producer. Ordinary disabled registration masks a
pre-enabled empty Line before committing disabled state; a software disable
depth alone would not close the existing controller gate. This is delivery
control, not proof of producer shutdown or drained device activity.

Shared detach must first stop this consumer's producer and exclude restart,
then unlink/drain its action before freeing callback resources. Surviving
shared owners keep their delivery policy. Descriptor mappings remain independent
of action ownership; neither this ordering nor action drain resolves MSI device
quiescence and allocation-release obligations. PerCpu and NMI keep their
separate source-owner exclusion requirements.

`runtime/entry.rs` owns the trap-facing and generic IRQ/NMI entry sequence;
`runtime/manager.rs` owns mapping, registration, control and removal transactions.
Explicit crate-root exports expose these APIs from their owning modules rather
than through a manager export aggregator. This is a visibility and responsibility
boundary, not a new execution-context proof.

The public `handle_irq` and `handle_nmi` raw entries are unsafe. The HAL adapter must
establish the required current-CPU, IRQ-mask and claim conditions before calling
them; the functions do not create hardware-entry provenance or mask IRQs.
Ordinary claim processing retains IRQ exclusion. Only after completion may the
deferred tail temporarily enable IRQs; it stays CPU-pinned and restores the mask
before watching exit. Dedicated NMI entry does not permit those windows.
`runtime/rcu.rs` is the unsafe watching/read bridge and relies on initialized CPU
storage, exclusion of migration and ordinary IRQs, balanced same-stack exit,
and completion of protected work before exit. `PendingIrq` is the sole claim
owner, but its non-Send marker does not itself establish these entry conditions
or certify that an arbitrary completion cookie is a valid controller claim.

`kcpu::excp::{IRQ, NMI}` store unsafe callback pointers and HAL implements that
same contract. Architecture call sites justify entry masks and captured state.
`IntrManagerIf::{dispatch_irq, dispatch_nmi, complete_irq}` preserve unsafe
signatures through the `kiface` facade and provider signature checks. This
makes the cross-crate proof obligation explicit without adding locks or a
second interrupt-entry token. A safe wrapper cannot silently invent it.

`PendingIrq::new` is unsafe: construction transfers a unique acknowledged claim
or a backend-defined no-completion sentinel. The constructor's caller must
retain controller validity and same-CPU IRQ/migration exclusion through explicit
completion or Drop, within the initialized CPU's watching hardirq/NMI entry.
Its non-Send marker prevents thread transfer, not migration
or the release of an outer guard; those remain explicit unsafe obligations.
Completion consumes the original cookie once. No guard can establish that a
fabricated cookie belongs to a real hardware claim. Kernel fixtures instead use
a crate-private, unittest-only simulated claim with no hardware obligation;
their entry helper establishes CPU/IRQ and hardirq context. They do not validate
physical EOI/deactivation by writing fabricated cookies to a controller.

`IrqStateDesc::flow: Option<IrqFlow>` starts at `None` for an unbound candidate.
Only backend selection establishes a production flow: fixed local sources use
PerCpu, and supported external sources use explicit Level, Edge or Fasteoi
capabilities. Unknown or foreign sources return `UnsupportedFlow`. There is no
production Legacy flow or dispatch fallback. An unbound descriptor cannot invoke
actions; an arriving claim still requires its original completion.

`generic_handle_irq` and `generic_handle_nmi` are safe consumers of an owned
claim. The constructor's entry-scope obligation supplies CPU/IRQ exclusion and
watching through consumption, without another token or public unsafe call.
Normal dispatch rejects effective NMI context before taking flow locks; NMI
dispatch establishes its own effective NMI scope. Neither entry authorizes
fabricating a claim or ending its required entry scope early. PerCpu action
borrows require source-owned exclusion of reclamation until the entire dispatch
returns, including when a handler disables its own source.

`cfg(unittest)` fixtures use `IrqStateDesc::test_new` with an explicit fake
Fasteoi chip or another explicit test flow. Those fixtures do not establish a
real backend binding or a production fallback. The legacy boolean enable bridge
and force-enable semantics remain separate control APIs and do not bypass
backend source validation.

## Ordinary Level Admission

`src/runtime/dispatch.rs` retains the original Line through mask/ack, actions
and conditional unmask. Level hooks run under its short lock and must be bounded,
non-sleeping, non-reentrant and ordinary-IRQ-safe; lock order is Line then chip.
Actions run unlocked. A rejected overlapping claim cannot clear another owner's
running state. Disable or teardown during a handler prevents tail unmask.
`src/runtime/resend.rs` never queues level replay: the provider must guarantee
that an asserted source can reassert on unmask. Neither pending nor in-flight
proves device quiescence, posted-delivery drain or completed hardware shutdown.

## Configuration Failure Boundary

`src/backend/platform.rs` propagates the controller's configuration result.
`src/runtime/manager.rs` publishes new regular/shared action ownership only
after configuration succeeds, and commits enable/depth/configured generation
only after successful application. `src/runtime/nmi.rs` similarly excludes
the sole Line-owned NMI action publication on configuration failure. Existing
actions remain owned; failed configuration is not permission to destroy them.
Resolved mapping/metadata and partial hardware writes are not rolled back.
The provider must not restore delivery on failure, and a caller must treat
the error as requiring recovery even when logical enable state is unchanged.
This is not hardware-active drain or a guarantee that every provider
performs readback; those guarantees remain controller-specific.

## irq_work Ownership And NMI Boundary

The trusted platform must return a permanent notification function that never
locks, allocates, logs, lazily initializes or invokes callbacks inline. It must
notify only the prepared current CPU at ordinary IRQ priority. Configuration
and CPU-slot prerequisites are established before NMI consumers are exposed.
Ready is release-published after provider setup; NMI readers acquire readiness
without ordinary IRQ context locks. Failure leaves the node caller-owned.

Unsafe queue callers own the node's stability, initialization and callback
contract. A successful claim serializes producers; the next pointer is private
until release publication. The acquire-detaching local consumer alone reads
links afterward. Duplicate callers need independent external lifetime protection:
AlreadyQueued is not a lifetime lease, and a freed pointer cannot be retried.
Neither producer nor dispatcher may access storage after its ownership handoff.

The static CPU template contains only null pointers, false atomics and None;
its StaticSlotValue implementation introduces no shared owned allocations.
UnsafeCell notification storage is immutable after initialization; backlog is
accessed only by the IRQ-off, non-reentrant local consumer. NMI writers modify
only the atomic incoming list. Callbacks are trusted not to enable IRQs, sleep,
unwind, or wait for work whose execution requires this CPU. A callback failure
is fatal kernel behavior, not a recoverable queue transaction.

Do not run draining from NMI exit after merely clearing software depth: the
interrupted execution may still hold an ordinary lock. The pending ordinary
IRQ waits for restoration of the original mask. Long IRQ-off sections, stopped
CPUs, permanent boot failure and hot-unplug have no reclamation-time guarantee.
Budget 64 bounds callback count, not callback cost or total interrupt utilization.
Malicious or unbounded callbacks can still monopolize the CPU; this is an
internal kernel capability, not an interface for untrusted code.

Tests separate deterministic CAS/ownership/budget interleavings from live
platform delivery. Passing a mock is not evidence of hardware NMI safety; live
tests must include NMI publication while the actual GIC lock is held.

## Interrupt Watching Boundary

`krcu_core::RcuContextIf` is a static contract implemented by `krcu`, including
before GP service startup. Watching lookup must not lock, allocate, look up a
task or schedule; initialized CPU identity is required. It preserves the caller's
IRQ mask, never enables previously disabled IRQs and does not change NMI exclusion.
It is not a consumer read interface or a declaration of service readiness.
The provider's CPU watching reference has permanent storage but does not pin the
caller. Architecture IRQ adapters must exclude ordinary IRQs and migration
until all controller, deferred and lifecycle work finishes. Explicit RCU entry
exit occurs before the HAL can schedule. NMI scopes retain effective NMI state
through watching exit, including nested dedicated/classified paths. Their atomic
CPU-slot count requires initialized CPU storage and strictly nested same-CPU
execution; it takes no lock and never forms an exclusive reference shared with
NMI. Available stack space bounds nesting far below the address-sized counter.
Only coarse classification bypasses the ordinary context lock in NMI; detailed
snapshots are explicitly excluded. Watching alone grants no consumer borrow.
`runtime::rcu::with_nmi_claim` separately enters `krcu::with_interrupt_read`
after NMI context and watching are established, and leaves that read scope only
after original claim completion. Both callbacks must remain on the claiming CPU
without scheduling. An outer `NoPreemptIrqSave` spans both callbacks.
The typed Domain index is RCU-borrowed; the action is locally borrowed through
`NmiAction::with_current`, without locks or refcount updates. Watching alone
never permits reclamation. Directory retirement needs its grace period, while
action withdrawal requires completed local revocation. Synthetic tests verify
pairing, the read scope across completion, and lock bypass, not
hardware NMI entry or actual user instructions. The idle host separately brackets its
architecture wait; its preemption exclusion must span the IRQ return tail,
which must not access protected objects after watching exit. The
controller-classified claim path and its hardware completion remain source-audited;
the synthetic nested-scope test does not execute a real controller claim.
The user-return host restores watching before IRQ dispatch, rather than keeping
an unwatched CPU across the HAL scheduling tail. This does not supply x86 vector-2
NMI dispatch or repair its SWAPGS window; those architecture capabilities are not
advertised by the user-boundary integration.

## Softirq Group Registration Boundary

`open_softirqs` accepts trusted kernel enum/function pairs, not raw or external
data. The protected asset is `SOFTIRQ_ACTIONS`, the fixed action table. Partial
registration could leave a service exposing only one required BH priority; the
mitigation is validation and publication under the same IRQ-safe table lock.
Duplicates, occupied slots and excessive length return false without mutation.
No callbacks run under the lock, and no allocation or new unsafe operation is
introduced. The caller must prepare action dependencies before registration and
publish its own readiness separately. There is no runtime removal/drain guarantee.
Tests cover conflict rollback, successful group installation, repeated registration,
duplicate vectors, empty input and oversized input.

## Effective Context Boundary

`context::is_softirq_context` adds no raw-pointer or external-data input. It uses
the existing context snapshot and hardirq-first classification. This mitigates
executing BH-only callbacks inside a hardirq that interrupted softirq: an active
softirq nesting count alone is insufficient. CPU pinning and pass serialization
remain execution-provider obligations; the query changes no context state.

## 测试审计范围

测试审计范围包括 `ut-tests/` 的 guest 用例和 fixture，以及
`tests/host/unhandled.rs` 的四例 host 测试。guest 模块保持原可见性及
`cfg(unittest)`；host 模块使用 `cfg(all(test, not(unittest)))`。
两份未处理 IRQ 计数测试使用局部 `UnhandledIrqState`，不注册真实 IRQ。
迁移保留所有测试体、serial 标记和清理序列，不改变生产状态或 unsafe 契约。
`ScopedHardIrqContext` 位于 `ut-tests/bottom_half_context_test_support.rs`。
同文件的 `with_nmi_context` fixture 要求调用者初始化 CPU/current、排除普通
IRQ 与迁移，并保证回调正常返回且不调度。`ut-tests/bottom_half_context.rs`
保留持锁期间的 NMI 查询回归；`ut-tests/rcu_irq_entry.rs` 和
`ut-tests/rcu_context.rs` 保留主线 RCU entry 平衡回归。
管理器的两例 hardirq 拒绝测试使用 `ScopedHardIrqContext`，同时
维持 CPU 固定和本地 IRQ 屏蔽，不能退回仅增加上下文计数的 guard。
softirq 的作用域 fixture 位于 `ut-tests/bottom_half_softirq_test_support.rs`；
二者仍通过原 `test_support` 模块提供测试期访问。

## 信任模型

测试辅助 `ScopedHardIrqContext` 自己持有 `NoPreemptIrqSave`，先固定 CPU 并
关闭本地 IRQ，再增加 hardirq 深度；析构时先恢复上下文计数，再恢复 IRQ 和
抢占。仅设置 per-CPU 计数不足以模拟 hardirq，否则锁释放触发的任务切换可能
让其他任务继承错误的中断上下文。该辅助仅在 unittest 配置编译。

替换 deferred executor 的测试通过 `ScopedDeferredExecutor` 保存并恢复原入口，
同时覆盖注册状态和 IRQ-tail 原子快路径。测试结束只清空入口会永久切断已启动的
softirq 执行，导致真实设备已经置位的完成事件无人处理；作用域恢复在重新允许
本地 IRQ 和抢占之前完成，不改变生产注册策略。该辅助持有的 `NoPreemptIrqSave`
只屏蔽测试所在 CPU 的中断，hook 槽位是全局可见的，其他 CPU 上的真实 IRQ 尾部
仍可能并发调用已注册的测试入口，因此对调用计数的断言必须容忍额外调用，只验证
测试自身触发的调用确实发生过。

`kirq` 不直接暴露用户态 ABI。它信任内核调用者提供的 handler 和 descriptor
符合执行上下文约束，同时把以下边界视为外部依赖：

```text
内核驱动 / 子系统
   │  IrqDesc + Arc<dyn IrqHandler>
   v
kirq
   │  IntrManagerIf
   v
平台中断控制器后端
   │  MMIO / CPU priority / EOI / IPI
   v
硬件中断控制器
```

默认前提：

- 平台后端正确实现 `IntrManagerIf` 的 configure、enable、dispatch、complete 和
  notify ordering；
- 平台后端把 normal dispatch 映射为 OS-visible virq，并且不会在 MSI mapping miss
  时把 raw vector 当作 virq 派发；
- MSI backend 正确实现 `MsiBackendIf` 的 vector allocation、free 和 message
  composition；
- IRQ handler 可以在 hardirq 上下文运行；
- NMI handler 满足更严格的 pseudo-NMI 执行约束；
- lifecycle hook 和 deferred executor 都只由一个明确 owner 安装；当前 deferred
  executor owner 是 softirq。
- `device_res` / devres 是驱动框架适配层，不能成为 `kirq` 的依赖。驱动资源到
  kernel IRQ core 的转换由 `kdriver::resource` 负责。
- `kbuild_config::NR_CPUS` bounds CPU-local control, NMI publication and
  participant storage. Configured capacity is not proof of CPU readiness,
  online membership or completed source shutdown.
- `IrqAction`、`IrqDescRuntimeState` 和 `IrqActionSnapshot` 是 `kirq` 内部状态
  边界。驱动、devres 和 irqchip backend 不能直接复用或修改这些结构。
- Generic IRQ line/source subscriptions are not provided. Device subsystems
  own readiness and waiter wakeup; they may submit prepared deferred work.

## 外部边界 / 攻击面

1. **设备中断输入**
   真实硬件或模拟设备可以触发 IRQ。`kirq` 不信任设备行为，只按平台后端
   claim 出来的 interrupt id 进行 descriptor lookup 和 handler dispatch。

2. **固件/平台 metadata**
   device tree、ACPI 或平台静态表会生成 `IrqDesc`。错误 metadata 可能导致
   trigger、polarity、domain 或 hwirq 配置错误。

3. **平台控制器后端**
   GIC/APIC/PLIC 后端负责 MMIO、priority、EOI/deactivate 和 IPI 发送。`kirq`
   通过 trait contract 约束它们，但不直接验证 MMIO side effect。

4. **MSI backend**
   x86 APIC 等后端负责把 controller-local vector 和 CPU/APIC destination 编码成
   device-visible MSI message。`kirq` 只保存 virq 映射和消息载体，不解析具体
   架构格式。

5. **内核 handler / hook 回调**
   regular handler、NMI handler、lifecycle hook、waiter wake 和 deferred
   executor 都是内核提供的回调。IRQ core 只管理调用顺序，不验证回调内部是否睡眠
   或拿错锁。

6. **trap adapter 边界**
   `kirq` 不注册 CPU trap handler。`khal::irq` 必须在进入 `handle_irq()` /
   `handle_nmi()` 前建立正确的 trap-entry 执行约束。

本模块不直接处理用户指针、DMA buffer、文件内容、网络报文或 FFI。

## unsafe 代码清单

The private `handle_level_irq`, `handle_edge_irq`, `handle_fasteoi_irq` and `handle_percpu_irq`
boundaries in [dispatch.rs](../src/runtime/dispatch.rs) require `entry` to lock
the exact supplied `line`, with `virq` identifying that same Line. Their callers
derive both identity and owner from lookup and obtain `entry` from `line.lock()`;
numeric IRQ equality alone is insufficient. Accepted ordinary execution establishes
running admission on that Line before snapshot capture or unlock. The separately unsafe
`IrqDispatchGuard::snapshot_actions` requires that exact retained-Line lock and
either live ordinary running admission or a PerCpu execution protected from
withdrawal by the source owner. Every cursor access must precede the end of that
execution; the returned cursor borrows the guard, which alone does not prove
source shutdown. Rejected execution keeps the lock through completion and
captures no action cursor or execution credit.

`runtime/manager.rs::try_free_percpu_irq` is an unsafe reclamation boundary.
Before calling, the owner must stop every participating CPU's source and wait
for all admitted dispatches to return, including software dispatch, their handlers,
original completions and post-completion tails;
restart and new dispatch must remain excluded until return. Its enabled-slot
check is necessary but cannot validate this lifetime proof. Failure to complete
shutdown requires retaining the action. No PerCpu execution wait or GP supplies
the missing exclusion.

The index boundaries are in `runtime/index.rs` and `runtime/state/registry.rs`:

The registry is a private child of `state`, so its atomic metadata/index
transactions can access Line identity and lifecycle fields without widening
those fields to sibling modules. The state facade re-exports required operations
internally, not a root-level registry mutation API. Child visibility is not
synchronization: registry exclusion and the original Line lock still protect
identity checks, flow/descriptor commit and paired index updates. This boundary
does not establish action-reader quiescence or hardware shutdown.

- Prepared XArray insertion runs in allocator-initialized boot/task context,
  never IRQ/NMI/BH context. NOWAIT preparation precedes registry/Line exclusion.
- `withdraw_msi` requires successful RCU submission preflight before exclusion.
  MSI publication allocates both empty retirement records before publishing the
  Line; failure returns the unpublished reservation for backend rollback.
  Release consumes those records only after validation, without allocating.
  Every returned retirement record must be submitted after restoring that same
  context, before invoking arbitrary backend callbacks.
- Both owning indexes retain the original Line. Withdrawal marks it retired
  before removing either entry, so a racing old lookup cannot gain admission.
- IRQ readers clone their Arc within an interrupt RCU scope. Numeric NMI queries
  only borrow and return a scalar; they cannot run a final Line destructor.
- Retired tree nodes and index-owned Arcs are released by RCU, not leaked.
  Remaining owned Arc holders extend lifetime beyond the grace period.
- The unittest-only quiescent fixture removes a synthetic slot without pruning
  radix nodes. It requires exclusion of same-key borrowed readers and writers;
  it is not used to prove the production retirement protocol. The runtime
  paired-index reuse test instead exercises actual withdrawal and RCU callbacks.
`arch/kirq/src/bottom_half/context.rs` 使用 `kcpu_slot` 的
`CpuSlotCell<SpinNoIrq<IrqContextState>>`：

- `current_state_snapshot()` 在 `NoPreemptIrqSave` pin 下取当前 CPU 的 slot，并经
  `SpinNoIrq` 共享读取 context snapshot；
- `with_current_state_irqoff()` 在同一 pin 下经 `SpinNoIrq` 更新当前 CPU 的
  context depth。

对象排他性由 `SpinNoIrq` 建立；`NoPreemptIrqSave` 只负责固定 CPU 并屏蔽本地 IRQ
重入。hardirq guard 和 softirq IRQ-tail hot path 使用 crate-local `*_irqoff`
helper，调用方必须已经建立 local-IRQ-masked + CPU-pinned 上下文。因此普通任务查询
不会与本地 hardirq guard 并发读写同一个 per-CPU slot，IRQ-tail hardirq/softirq
也不会重复保存/恢复 IRQ state。

`arch/kirq/src/bottom_half/softirq.rs` 使用 `SOFTIRQ_PENDING.current_ref_raw()` 访问当前 CPU 的
per-CPU atomic pending mask。调用方在访问前 pin 当前 CPU；pending mask 自身是
`AtomicUsize`，用于处理 IRQ/softirq 之间的 pending bit 发布和获取。

其它相关 unsafe 边界位于模块外：

- 架构 trap 入口由 `kcpu` 汇编和 trap dispatch 宏进入 `khal::irq` adapter；
- 平台 IRQ backend 在 `drivers/platform/irq` 中执行 MMIO、priority mask 和必要的汇编屏障；
- `kiface` 把平台实现绑定到 `IntrManagerIf`。

因此本模块的主要安全责任不是局部内存安全，而是保持上下文、锁顺序和 completion
语义正确。

## 内存安全不变量

The ordinary Level/Edge/Fasteoi action proof is traced in
[Ordinary Action Lifetime Proof](design.md#ordinary-action-lifetime-proof):
[action_set.rs](../src/runtime/action_set.rs) owns node/link invariants,
[dispatch.rs](../src/runtime/dispatch.rs) binds cursors to execution admission,
[state.rs](../src/runtime/state.rs) serializes admission and its zero predicate,
and [manager.rs](../src/runtime/manager.rs) retains writer exclusion through
unlink, drain and node recovery. A new action reader must participate in this
admission protocol; an unrelated RCU read scope is not sufficient protection.
Do not clear detached next links or reconstruct exclusive node ownership until
drain. Do not permit another membership writer to reclaim a reachable successor
during that interval. Acquire/Release ordering and publication ceilings alone
cannot discharge either lifetime obligation.

PerCpu uses the claiming CPU's local admission predicate. Rejected claims
complete under the retained Line lock without borrowing an action. Accepted
claims run and complete unlocked, with no execution count or running gate;
the source owner protects the raw action borrow until the entire dispatch returns.
Accepted Level/Edge/Fasteoi and replay own `is_running`. The ordinary drain
predicate is `!is_running` under the Line lock; pending queue entries own no
execution admission. Ordinary idle is not a PerCpu completion certificate, irqchip-active
query, hardware quiescence proof or substitute for producer shutdown. Removing
the action GP does not remove these obligations or alter XArray directory RCU,
the CPU-local NMI owner protocol, or the generic `krcu-list` contracts.

- Regular/shared handlers live in the retained Line's private stable-node action
  chain. Dispatch captures a cursor and publication-sequence ceiling under the
  Line lock, with snapshot lifetime bound to the dispatch guard. It clones
  neither a list Arc nor handler owners. Callbacks borrow actions after releasing
  short locks; action traversal requires no RCU read scope.
- Accepted Level/Edge/Fasteoi handlers and replay establish running ownership
  before snapshot capture and unlock. Rejections hold the Line lock through
  chip operations and original completion without execution admission. Normal-IRQ
  claims for NMI-owned Lines also complete under the held Line lock without
  admission or action access. PerCpu rejection also completes under the held
  Line lock without a count. Accepted PerCpu borrows instead rely on completed
  source-owner shutdown before unsafe removal, not ordinary execution drain.
  `free_irq()` drains admitted claim and running replay obligations before
  recovering the detached node and returning handler ownership. This execution
  drain covers every action borrow and original completion; directory RCU is a
  separate lifetime boundary, not the action reclamation mechanism.
- 内部 `WakeThread` return 分类在当前里程碑是 inert future state，不能调度 task、
  raise softirq 或创建 workerqueue work。
- `PendingIrq` cannot be sent across threads. Its remaining host completion
  must be consumed on the claiming CPU, exactly once; the integration's CPU
  exclusion, not the non-Send marker alone, preserves that affinity.
- `IntrManagerIf::complete_irq(cookie)` must consume only the original claim's
  remaining host-owned completion obligation, on its claiming CPU. The backend
  must obey the source's acknowledgement/deactivation protocol rather than infer
  it from trigger type alone. A no-op tail or guest-owned deactivation is not a
  hardware-quiescence certificate; forwarding shutdown needs a separate proof.
- `IRQ_STATE` 中的 `virq -> IrqStateDesc` 和 `domain -> hwirq -> virq` 映射必须保持一致。
- 带 domain 的 descriptor 只能使用 `domain/mod.rs` 静态 registry 中已注册的 domain；
  未知 domain 必须返回 `IrqDescError::UnknownDomain`，不能创建数据面不可解析的映射。
- Firmware 中断只能通过 `domain/fw.rs` 中按 `FwNode` 精确身份注册的
  controller/domain 关联映射。`FwNode` 相等性同时比较 backend 身份和 node id；
  不能退化为 compatible 字符串、`IrqController` family 或数字 id 匹配。
- `IrqDomainXlate::translate()` 只把绑定 cell 解码为 `IrqHwSpec`，不能分配 virq、
  选择其它 controller 或回调 `kirq`。registry 锁必须在调用 translate 和
  `try_map` 之前释放。
- `register_irq_domain()` 是 boot-only 且非幂等：同一 controller 的第二次关联、
  `Existing` 目标已有关联、`Existing` 目标不存在、新 domain 分配失败都必须显式
  报错，不能静默覆盖或回退到其他 root。关联保存在所属 `IrqDomain` 内，不存在
  独立的固定容量关联表；boot 期由注册锁串行化。
- `IrqDomain` 的 controller family 是 domain 元数据。`register_irq_domain()` 的
  `Existing(id)` 保留该 domain 已有的 kind；`New(kind)` 只在新 domain 创建时
  一次性初始化 kind；任何路径都不能覆盖已存在 domain 的类型。`try_map_fwspec()`
  使用该权威值填充 descriptor，避免 domain 与 controller kind 的矛盾组合。
  每个 firmware controller 实例获得独立 domain；同族第二个实例获得新分配的
  动态 domain，不再与第一个实例共享命名空间。family 只是元数据，不代表任何
  驱动有权操作该实例：访问单一已初始化控制器的驱动（当前 GIC/PLIC）必须只接纳
  该实例注册的 domain id，不得用 family 相等代替实例归属。真正支持第二个硬件
  实例还需要按实例保存寄存器基址/chip 数据的后端。
- `register_irq_domain()` 只允许启动控制面调用，禁止在 IRQ/NMI/BH handler 中
  调用。`New` 额外要求 allocator 已就绪并使用非睡眠 `NOWAIT` 分配，失败返回
  `NoMemory`；`Existing` 不分配内存。
- 旧 root 的选择是启动接线的显式决定，不在通用注册中根据 controller family、
  注册顺序或布尔参数推断。`IrqDomainTarget::Existing(id)` 只验证目标存在且尚无
  固件关联；`IrqDomainTarget::New(kind)` 分配新实例 domain。family 和 identity
  策略属于被选中的 domain 本身，不是调用者另传的覆盖参数。
- identity fallback 只属于被显式选中的 well-known root，是旧数值编号的兼容策略。
  `create_domain()` 创建的动态 domain 一律 `is_identity = false`：未显式映射的
  hwirq 必须解析失败，不能退化成同号全局 virq 并取到其他来源的 Line。
- `runtime/index.rs` 中 `LineIndex` 的两种存储必须区分：借用视图
  （`Storage::Shared`）是无状态、无计数的视图，可临时构造；只有测试私有
  registry 的 `Storage::Private` 持有 `count`。发布/撤销的计数更新只作用于私有
  对象，借用视图不得因一次临时查询而增减计数，否则删除会下溢或回绕。
- `try_map_fwspec()` 必须区分“无声明”（backend 返回 `Ok(None)`）、声明损坏
  （`MalformedSpec`/`InvalidHwirq`）、绑定不支持（`UnsupportedSpec`）、
  controller 未注册（`ControllerNotReady`）和 IRQ core 拒绝（`Map(...)`）；
  不得回退到默认 controller 或 family 推导。
- `MappedIrq::desc` 必须来自 `commit_resolution` 返回的已存储 descriptor，使
  translate 得到的 trigger/polarity/controller 不会被 consumer 默认值覆盖。
- `try_disable_irq_nosync()` 必须保持 lookup-only；未知 IRQ 返回
  `IrqDescError::UnknownIrq`，不能在 hardirq-safe disable 路径中创建 descriptor 或
  domain mapping。
- RCU protects borrowed Domain/directory entries across withdrawal. Writers
  prepare capacity before exclusion, publish initialized Lines under their lock,
  and submit withdrawn ownership only after restoring task context.
- MSI allocation 必须只向调用者暴露 virq 和 `MsiMessage`；backend-local vector、
  APIC id 和 affinity 选择不能泄露到 `device_res` 或 generic driver。
- NMI dispatch 不得依赖 `IRQ_STATE`，否则 pseudo-NMI 可能打断 normal IRQ 持锁路径并
  自锁。
- lifecycle exit hook 必须与本次 enter 捕获的 hook 配对，不能在处理中途因全局 hook
  被清理而丢失。lifecycle 表示 IRQ-on trap/preempt-off 生命周期，不是
  hardirq-depth 边界；irqchip 后续把 claim 分类为 NMI 或 spurious 也不改变这层配对。
- hardirq depth 必须由 `HardIrqContextGuard` 配对维护，并在 irqchip dispatch（包括其
  generic IRQ/NMI helper 和 controller completion）返回后、deferred executor 运行前
  退出。
- `LocalBhGuard` 必须在整个 BH-disabled 生命周期内持有 `NoPreempt`，并且不能跨线程
  移动。这样 enter/drop 和 outermost drain 都绑定在同一 CPU 的 per-CPU depth 上。
- deferred executor 只能在 irqchip dispatch 返回且 generic hardirq depth 已退出后运行；
  若存在 IRQ/NMI claim，其 generic helper 必须已经完成 controller completion。executor
  cannot retain `PendingIrq` or alter claim completion ownership.
- softirq pending bit 不能通过 `load + store(0)` 获取；runner 必须使用 atomic
  `swap(0, Acquire)`，raise 必须用 `fetch_or(Release)`，避免 handler 期间重新 raise
  的 work 被覆盖。

## 线程安全

### normal IRQ state

`IRQ_STATE` serializes directory and Domain writers; each Line lock protects
descriptor, action and admission state. Normal dispatch retains the original
Line through an interrupt RCU lookup, then takes only its Line lock. Driver
handlers run after releasing that lock. Completion uses the retained identity,
never a fresh number lookup.

Domain and logical indexes own the same Line. Reverse conflict checks use its
descriptor and the actual Domain binding instead of a redundant metadata map.
A reader racing withdrawal either misses or retains the old Line and observes
its retired gate; number reuse cannot redirect that retained owner. Numeric-only
IRQ/NMI translation uses an RCU borrow. Early boot and control reads use the
XArray writer lock, without assuming a current task or a running RCU service.
Waiting teardown APIs (`disable_irq()`, `synchronize_irq()`, and `free_irq()`)
are valid only from ordinary kernel context. They reject hardirq, softirq, and
BH-disabled callers before waiting on aggregate execution, because those contexts may be
the same execution path that must exit to make progress.
Callers must also avoid holding locks or resources that their IRQ handler may
acquire while waiting; otherwise teardown can wait for a handler that is blocked
behind the caller's own lock.

All descriptors survive action removal and empty-claim completion, including
domainless fixed Lines. Domain-bound Lines retain both owning XArray entries.
Mapping ownership is not inferred from
handler count, and handler removal cannot undo preparation. General non-MSI
runtime disposal is not provided yet: these descriptors retain registry lifetime.
MSI resources retain their separate explicit release transaction.

Retaining a Line is not admission to use its hardware binding. Management
waiters retain the original Arc and recheck registry identity after locking;
dispatch retains identity and execution admission; queued replay retains identity
and queue membership only, acquiring admission at eligible execution.

Logical identity is assigned through `klazy::Once<Virq>` after recoverable
commit validation and before index publication. Only the registry/Line-locked
commit path initializes it, with a non-reentrant integer-only closure. Readers
use non-waiting `get`, never initialization or waiting operations; they must
still retain or borrow the Line through an appropriate lifetime owner.
The assigned number survives retirement and does not authorize controller or
action access. Reusing one Line under another logical number is rejected.
The stored integer adds no destructor work or client-owned resource.

Explicit disposal marks the Line retired under its short lock before removing
the indexes. This one-way state rejects republication of the same allocation,
new management/dispatch admission and action installation or execution. It is
not cleared by normal action teardown, and a failed release must not set it.
The original claim still requires completion when a retired lookup is rejected;
no old-Line flow operation may be invoked on that path. Retirement does not
itself provide RCU protection or physical interrupt drain.
Unittest-only checkpoints observe partial publication and withdrawal without
releasing the writer or Line lock. Their interrupt observers perform lookups,
not handler admission, controller operations or sleeping waits. Nested NMI
observers return scalar identities only. The cross-CPU interrupt-reader fixture
keeps borrowed storage alive until its read scope exits and drains callbacks
only after releasing the reader. These checks do not establish device shutdown
or hardware late-message ordering.
Final storage destruction must not contain a client action or an unconsumed
hardware-release obligation. The management path returns removed handlers
outside its locks instead of delegating their destructors to storage retirement.
Completion registrations and deferred wakes preserve their own borrow/owner
lifetimes; dropping a Line is not a wake or cancellation operation.

Empty-Line deallocation relies on the non-sleeping allocator and on completed
mutex-listener cleanup, not merely on the presence of an Arc. The pinned
no-std Event implementation releases its inner storage without acquiring the
management mutex. Changing that dependency, adding a destructor-bearing Line
field, or making allocation release sleepable requires a new release-context
audit. IRQ-safe allocator locks are not NMI-safe: a future NMI lookup must not
become the final owning release of a retired Line. Numeric NMI lookup therefore
borrows through RCU and returns only the logical number. Normal dispatch retains
an Arc and checks retired admission under the Line lock.

MSI allocation publishes a fresh `(MSI_DOMAIN, backend_vector) -> virq`
mapping under `IRQ_STATE`, retaining its exclusive reservation in the Line.
Ordinary `unregister()` does not delete MSI descriptors or mappings. Final
`free_msix()` first validates unused, logically disabled state and reservation
ownership, then withdraws the descriptor and reverse mappings before returning
the vector.
An actionless Line can still be enabled by the legacy force-enable API; that
state is rejected without changing its mappings, control state or reservation.
Logical disablement is necessary but does not prove device-message quiescence,
which remains the resource owner's obligation.
Pre-withdrawal rejection preserves the caller's cleanup identity. Backend
release consumes valid ownership and has no recoverable post-withdrawal failure.

### Line-owned NMI Action

`src/runtime/nmi.rs` installs one `KBox<Handler>` in
`IrqLine::nmi_action: NmiAction`; `src/runtime/nmi/action.rs` owns its raw-pointer
publication and recovery invariants. NMI registration stores no second Arc in
the ordinary fanout list. The locked ownership marker contributes one action to resource
accounting and excludes ordinary/token free and shared installation. Owner
allocation precedes short locks; rejected ownership is released after guards.

Only successful local preparation release-publishes the current CPU's pointer.
The private scoped reader retains CPU/IRQ exclusion and cannot leak a borrow.
True NMI lookup retains the typed Domain index through original completion and
cannot fall back to another namespace on a miss. Normal-IRQ claims for this Line
complete under the already-held Line lock without execution admission and never borrow
the local owner or invoke it with the logical-IRQ ABI, regardless of local enable
state. Neither path performs action-owner reference counting. True NMI entry,
including IRQ-on NMI, is unchanged; no supported production backend required the
removed software/test handler compatibility behavior.

Removal validates sleepable task context, enters Closing under Line exclusion
and refuses enabled, prepared or published local slots. The private unsafe
`NmiAction::take` checks publication under management/Line exclusion, preventing
concurrent installation or preparation. It recovers ownership only after each
local revocation has completed; no callback service or action GP is involved.
The management helper returns the owner outside its guards for destruction.

`src/runtime/nmi/shutdown.rs` retains the original Line and unique shutdown
claim. Ordinary unregister and replacement cannot consume its action while
the claim exists. Finish rechecks identity and local obligations; rejection
returns the handle for retry. Dropping an unfinished handle abandons only the
claim and leaves Closing and the action intact. Forgetting it prevents retirement.
Handle Drop takes the Line lock and must not run in NMI. Shared callback captures
must be destroyed before recovering unique ownership for finish.

The participant snapshot records enabled/prepared slots, not all possible
starters. The source owner must stop producers, exclude restart, complete
target-CPU disable/teardown and exclude pending/active claims across reuse.
A queued callback is not an acknowledgement, and its acknowledgement is not
completion of the surrounding IPI claim. The PMU owner's all-start-capable-CPU
stop pass also drains old pinned start transactions before restoration.
Local controller success does not prove remote shutdown. Conversely, an empty
slot is meaningful only because unsafe local teardown already established
quiescence. Unrelated NMIs need not stop. Synthetic fixtures do not establish
physical NMI delivery or hardware late-claim isolation.

Shared action removal unlinks the matching token from the chain; it does not
leave a vacant slot. Tokenless removal accepts only non-shared membership before
changing the action count or oneshot state. A rejected removal must preserve
the shared owner, its token, dispatch eligibility and the Line's enabled state.

NMI rejection in checked normal management APIs happens before any ordinary
IRQ lock. Compatibility wrappers do not log this rejection in NMI. This does
not make arbitrary handler destructors NMI-safe: passing ownership into an
invalid-context call can still drop the rejected input on the caller's stack.
The caller remains responsible for the destruction context of its inputs.

### Lifecycle hooks

`IRQ_LIFECYCLE_HOOKS` 只保存两个函数指针。`IrqLifecycleGuard::enter()` 复制 hook
快照，并把 exit hook 存入 guard。回调不在锁内执行。hook 运行于 trap/preempt-off
生命周期内，但不作为 hardirq-depth 状态来源。

### IRQ context state

`IRQ_CONTEXT_STATE` 是 per-CPU 普通计数状态。查询和 guard 更新都使用
`NoPreemptIrqSave` 或明确的 IRQ-off 调用约束保护当前 CPU slot。public 查询自己
建立 guard；hardirq 和 softirq IRQ-tail hot path 复用调用方已经建立的
local-IRQ-masked + CPU-pinned 上下文。

hardirq、serving-softirq 和 BH-disabled 都被视为 non-sleepable / interrupt-like
状态。`interrupt_context_level()` 显式区分 BH-disabled 和普通 task context，避免
diagnostics 把 BH-disabled 误报成可睡眠上下文。未来 IRQ thread 是 sleepable context，
必须另建执行上下文边界，不能复用 `is_in_interrupt_context()` 作为 thread-state 标记。
异常 context 诊断使用原子计数并限流 warning，避免 hot path 中的错误状态造成日志风暴。

### Deferred executor

`DEFERRED_EXECUTOR_HOOKS` 只保存一个 hardirq-exit 函数指针，并用 `SpinNoIrq`
保护注册/清空控制面。runner hot path 从 atomic function-pointer slot 读取 hook，
不获取注册锁。executor 运行在 normal hardirq exit / NoPreempt 上下文，不能睡眠、
不能依赖当前进程/task 上下文、不能递归调用 deferred runner，也不能获取可能由被
中断上下文持有的 sleepable 锁。

当前不加入全局 reentry guard，因为它会在 SMP 上抑制其它 CPU 的合法 hardirq-exit
handoff；softirq pending state 使用每 CPU pending mask 和 context gating。

### Softirq state

`SOFTIRQ_PENDING` 是 per-CPU atomic bit mask。softirq runner 通过 atomic exchange
获取 batch，action table 在 `SpinNoIrq` 下 snapshot，handler 在锁外执行。runner
入口/出口保持 local IRQ masked，但 action loop 内临时打开 local IRQ，并在检查新
pending bit 前重新关闭。`ktask` 提供 per-CPU `ksoftirqd/N`，但 `kirq` 只通过
`SoftirqDaemonIf` 请求唤醒当前 CPU daemon，不创建任务或阻塞。hardirq、active
softirq 和 BH-disabled context 中 raise softirq 只保留 pending bit；direct runner
每批 action 返回后，若仍有 pending 且当前任务需要调度，或达到 restart 上限，
保留 pending 并唤醒 `ksoftirqd/N`；仅 restart 耗尽记录 restart-limit 计数。
`SoftirqDaemonIf::needs_resched()` 只读任务请求，不清除它，也不在 IRQ-disabled
runner 内执行调度。没有任务或未启用 preempt 时返回 false。
`raise_softirq_irqoff()` 期望调用方已经关闭 local IRQ；若 debug/UT 路径发现误用，
会记录诊断并临时关闭 local IRQ 后再访问 per-CPU pending/context state。
BH-disabled state 使用 `LocalBhGuard` RAII 管理，guard 持有 `NoPreempt` 并通过类型
约束避免跨线程 drop，从而防止在不同 CPU 上扣减错误的 per-CPU depth。

## 威胁分析

| 编号 | 威胁 | 触发条件 | 影响 | 缓解 |
|------|------|----------|------|------|
| T-01 | IRQ metadata 配置错误 | 固件或平台提供错误 trigger/polarity/domain | 中断丢失、重复触发或无法 mask | `IrqDesc` 明确携带 metadata，平台 configure 只消费规范化描述 |
| T-02 | handler executes under registry/Line locks | Dispatch invokes an action before unlocking | Callback reentry deadlocks | `dispatch_claim` and `replay` snapshot eligible actions, then release short locks before `run_actions` |
| T-03 | Mapping miss becomes identity dispatch | Strict Domain has no mapping | Wrong handler or completion | Direct Domain lookup preserves strict misses; only declared identity Domains may use the logical directory |
| T-04 | NMI interrupts an IRQ writer | A writer holds registry or Line exclusion | Same-CPU deadlock or premature handler destruction | NMI borrows the RCU-protected index and CPU-local action without locks; CPU/IRQ exclusion spans claim completion, local teardown revokes access, and removal refuses published slots |
| T-05 | claimed IRQ 未 complete | normal control flow 提前返回或忘记显式 complete | 控制器认为中断仍 active，后续中断异常 | claimed IRQ guard Drop 补偿 completion |
| T-06 | lifecycle enter/exit 不配对 | IRQ 中途 clear/re-register hook | hardirq nesting 或 deferred work 状态损坏 | guard 保存 entry 时的 exit hook 快照 |
| T-07 | hook/handler 睡眠或持有错误锁 | 调用者违反 hardirq/NMI 上下文约束 | deadlock、latency spike 或调度状态错误 | rustdoc 和设计文档明确回调上下文，IRQ core 不提供 sleepable dispatch |
| T-08 | IPI 请求先于内存发布被目标 CPU 观察 | 平台 `notify_cpu` 缺少顺序保证 | TLB shootdown 等跨核协议漏处理 | `IntrManagerIf::notify_cpu` 文档要求 publish-before-notify |
| T-09 | deferred executor 在 completion 前或 hardirq depth 内运行 | executor work 拉长 active IRQ、影响 EOI/deactivate 或被误判为 hardirq | interrupt tail latency、controller 状态异常或 context 诊断错误 | irqchip 的 generic helper 在返回前 complete；`handle_irq()` 随后固定执行 drop hardirq context、deferred、lifecycle exit |
| T-10 | 全局 reentry guard 抑制其它 CPU | SMP 上一个 CPU 的 executor 运行期间另一个 CPU 被跳过 | lost handoff opportunity | 当前不实现全局 guard；后续使用 per-CPU pending/reentry |
| T-11 | per-CPU context 非原子状态被本地 IRQ 并发访问 | 普通任务查询被 IRQ 打断，同时读写同一 CPU slot | context snapshot 不一致或 data race | public path 用 `NoPreemptIrqSave`，hardirq/softirq IRQ-tail path 只走已 masked/pinned 的 `*_irqoff` helper |
| T-12 | softirq pending 获取覆盖新 raise | runner 用 `load + store(0)` 清 pending | softirq work 丢失 | raise 用 `fetch_or(Release)`，runner 用 `swap(0, Acquire)` |
| T-13 | BH-disabled guard 跨 CPU drop | guard 生命周期没有 pin 当前 CPU 或可跨线程移动 | per-CPU BH depth 泄漏、underflow 或在错误 CPU drain softirq | `LocalBhGuard` 持有 `NoPreempt` 且不是 `Send` |
| T-14 | driver 读取当前 APIC id 生成 MSI message | MSI message 由设备层而非 IRQ backend 编码 | SMP/affinity/x2APIC 下中断投递到错误 CPU | `MsiBackendIf::compose_msi_message()` 是唯一 message composition 边界 |
| T-15 | MSI vector 与 handler 生命周期失配 | 设备释放 MSI resource 时 handler 仍注册，或 handler 注销提前删除 MSI mapping | vector reuse 后旧 handler、stale mapping 或 backend vector 泄漏 | `unregister()` 保留 MSI descriptor；`free_msix()` 检测 descriptor 是否仍被使用并完成最终清理；驱动 teardown 应先撤 handler 再释放 MSI resource |
| T-16 | descriptor 冲突被静默合并 | hwirq/domain/virq/trigger/polarity/controller metadata 不一致 | handler 绑定到错误 IRQ 或平台 configure 错误 | `try_resolve_desc()` / `try_merge()` 运行时返回 `IrqDescError`；两个确定的 trigger/polarity/controller 不会互相覆盖 |
| T-16b | Firmware controller 身份被猜测匹配 | 用 compatible、family 或数字 node id 代替 `FwNode` | 多实例或多 backend 时中断被路由到错误 controller | `domain/fw.rs` 只按 `FwNode` 的 backend 指针 + node id 精确关联；重复关联显式失败 |
| T-16c | 固件 cell 由错误 owner 解释 | IRQ core 猜测 trigger，或 controller 返回 logical IRQ | 绑定错误、双份 decoder 或 controller 越权 | `IrqDomainXlate` 只产生 `IrqHwSpec`；`kirq` 负责 domain 关联与逻辑映射；translate 错误按 `IrqXlateError` 分类返回 |
| T-16d | 新实例继承旧实例的数值 identity fallback | 动态 domain 仍带 `is_identity = true`，第二实例的 hwirq 未映射 | 未映射硬件号解析到全局同号 virq，命中其他来源的 Line | `create_domain()` 固定生成非 identity domain；只有保留 root 保留数值兼容；未映射请求返回 `None` |
| T-16e | 驱动按 family 接纳其他实例的 domain | 单例 GIC/PLIC 后端接受任意同族 domain，却通过 `desc.hwirq` 操作已初始化控制器 | 第二个实例的中断被配置到第一个控制器寄存器 | `is_gic_domain()`/`current_plic_domain()` 只接受当前实例持有的 domain id；family 相等不等于实例归属 |
| T-16f | 借用索引视图被当作持有计数的对象 | 每次修改临时构造 `LineIndex::shared()`，成功删除执行 `count -= 1` | 溢出检查下 panic；release 下计数回绕 | 借用视图无状态无计数；仅测试私有 registry 的 `Storage::Private` 持有计数；全局索引 MSI 分配—释放有回归测试 |
| T-17 | Missing MSI mapping becomes a raw logical IRQ | No `(MSI_DOMAIN, vector)` entry | Wrong handler selected | MSI dispatch uses the strict Domain Line index; a miss never falls back to the raw vector |
| T-18 | 未注册 domain 被映射成功 | 控制面接受未知 `IrqDomainId` | 驱动拿到永远不可达的 virq | `try_resolve_and_publish()` 在改状态前返回 `UnknownDomain` |
| T-19 | BH-disabled 被误判为普通 task context | diagnostics 只区分 hardirq/softirq/task | sleepable future callback 可能在 BH-disabled 区间运行 | `InterruptContextLevel::BhDisabled` 和 docs 明确其 non-sleepable 语义 |
| T-20 | context misuse 日志风暴 | underflow 或 hardirq 中反复 local_bh_disable | 日志淹没真实故障，拉长 IRQ 处理时间 | warning 计数保留，日志只输出初始样本和 2 的幂次样本 |
| T-21 | poll/future IRQ wake 被当作 future IRQ thread target | 后续 threaded IRQ 直接复用 poll/future wake bridge | 缺少 thread lifecycle、dev-id teardown、oneshot mask 和 scheduler ownership，导致悬空唤醒或错误 teardown | 文档明确 poll/future IRQ wake 只是临时 waiter notification；threaded IRQ、softirq 和 workerqueue 必须在 `kirq` 内形成独立所有权 |
| T-21b | Duplicate work execution | Concurrent claims or cancellation race | Corrupt callback state or leak in-flight ownership | Transfer: `kworkqueue_ng` owns synchronized claim/cancel/finish; `kworkerpool_ng` owns worker occupancy. |
| T-21c | Blocking from BH context | A BH callback invokes a synchronous wait | Deadlock or invalid scheduling | Mitigate: kirq exposes context predicates; workqueue waits reject non-sleepable contexts. |
| T-21d | Work storage released while borrowed | Producer or callback outlives storage | Use-after-free | Transfer: caller-owned pinned Work requires explicit drain; safe owned handles defer reclamation to the normal service. |
| T-21e | BH callback executed in the wrong context | Direct service entry outside target CPU softirq | Invalid per-CPU access or blocking | Mitigate: BH service validates CPU and softirq context; kirq owns softirq entry/exit. |
| T-21g | Callback dependency cycle | Callback waits on work that cannot progress | Bounded pool starvation or deadlock | Accept residual risk: optional self-wait checks catch local cases, not arbitrary dependency cycles; callers must avoid those cycles. |
| T-22 | oneshot runtime 字段被误当成已实现行为 | 后续代码只改 flag，没有安装 mask protocol | oneshot IRQ 无法屏蔽重入或误解除 mask | review 要求 oneshot mask 和 thread identity 在 `IrqDescRuntimeState` owner 内成组实现 |
| T-23 | threaded slot 提前保存 task/worker 指针 | 未建立线程生命周期、取消、CPU 亲和和退出同步 | handler return 后唤醒已释放对象，或在 hardirq 中触发 sleepable 路径 | 当前 `IrqThreadSlot` 不携带目标，带 slot 的 action 不会被当前 dispatch 路径同步执行；后续 milestone 必须先定义所有权和 teardown ordering |
| T-24 | Incomplete action/admission snapshot | Dispatch re-resolves state across locks | Withdrawal redirects an old claim or callback | Directory RCU protects lookup while dispatch retains the original Line's Arc; its lock grants admission and captures the action cursor and publication ceiling together. Traversal borrows the dispatch guard after unlocking; concurrent unlink may bypass unseen nodes. |
| T-25 | Stale controller updates | Concurrent register/enable/disable/free after directory release | A late unmask reopens a disabled or retired Line | The original Line lock spans state changes and controller application; teardown rejects enable and management admission pins identity through drain |
| T-26 | 驱动需要清 pending 后再开 IRQ，但 request 立即 enable | 复杂设备 request handler 后尚未初始化 DMA ring/MMIO 状态 | 中断 handler 观察未初始化设备状态 | `try_register_disabled()` 以 disable depth `1` 安装 handler，驱动完成初始化后显式 `try_enable_irq()` |
| T-27 | Handler remains active after teardown | Free recovers a detached node before execution admission drains | A handler accesses released MMIO/DMA state | Free preserves detached storage and next links until retained-Line drain completes; only last-action removal gates and masks the whole Line. Survivors remain eligible during non-last removal. Completion is published under the Line lock and waiters are woken after unlocking; IRQ completion never withdraws directory membership. |
| T-28 | 等待式 IRQ API 在 interrupt-like context 调用 | hardirq/softirq/BH-disabled 中调用 `disable_irq()`、`synchronize_irq()` 或 `free_irq()` | 当前 CPU 等待自己退出，导致死锁或长时间自旋 | `try_*` API 使用 `is_in_interrupt_context()` gating 并返回 `InvalidContext`；兼容 wrapper 记录 warning 后返回失败值 |
| T-28b | IRQ sync wait provider 无 sleepable current task | scheduler 尚未初始化或非 task context 调用等待 API | 非法调度或等待 API 伪成功 | `TaskBlockingContextIf::is_sleepable_context` 在 action 摘除前检查 sleepable current task，失败返回 `IrqDescError::InvalidContext`；释放控制锁后用 `CompletionWait` 栈上登记执行无失败等待，消除摘除后的登记失败分支 |
| T-29 | Wrong shared action removed | Adapter token mismatches action identity or is reused before synchronization | Wrong owner released or old handler uses freed state | Line-local tokens increase monotonically without reuse; free-by-token rejects PerCpu, unlinks first, then drains ordinary running ownership before returning the owner. |
| T-30 | shared 和 non-shared action 混用 | 普通 `register()` 后又注册 shared action，或反向混用 | dispatch/teardown 语义不明确，可能错误关闭 line | `try_register()` 只允许空 line，`try_register_shared()` 拒绝已有 non-shared action |
| T-31 | Device readiness published too early | Driver wakes readers before publishing data | Readers observe incomplete data | Device subsystems own data publication and wakeup ordering; IRQ core does not broadcast readiness |
| T-32 | Membership changes during removal wait | Another writer reclaims a successor still reachable from a detached node | A dispatch follows freed storage | The retained Line's management mutex serializes membership through unlink, execution drain and recovery. Detached next links remain intact until drain. Only last-action withdrawal adds whole-Line teardown; non-last survivors remain dispatchable. |
| T-33 | handler 无法识别触发的 IRQ | shared handler 回调没有 IRQ 参数，只能依赖外部状态推断 | 共享 line、diagnostics 或通用 wrapper 误判事件来源 | Normal IRQ and NMI handlers receive logical `Virq`. NMI obtains it from the retained Line's published identity; controller domain/hwirq selects the Line and is not the callback argument. |
| T-34 | Missing last-action mask | A teardown waiter is mistaken for a surviving action | The source remains unmasked without a handler | Mask the last action's source independently of teardown credits; retain descriptor ownership through and after drain |
| T-35 | teardown 期间失败注册仍修改 descriptor/mapping | 注册路径先 resolve/publish，再检查 teardown gate | 返回失败但留下 merged metadata 或新 domain mapping | 注册先做 lookup-only teardown gate，命中后不进入 resolve/publish |
| T-36 | teardown wait 窗口重新 enable action-less line | 最后一个 action 已摘除并 mask 后，另一 CPU 调用 `enable_irq()` / legacy force-enable | descriptor 被删除后平台 line 又 unmasked，后续 unhandled IRQ 或 storm；或失败路径留下 merged metadata/domain mapping | `try_enable_irq()` lookup-only 解析已存在 descriptor，并拒绝 `teardown_depth != 0` 或 `action_count == 0` 的普通 enable；legacy force-enable 在 resolve/publish 前先检查已有 teardown descriptor |
| T-37 | 等待式 teardown 持有 handler 需要的锁 | task context 持 driver lock 调用 `free_irq()` / `synchronize_irq()`，旧 handler snapshot 在另一 CPU 等同一把锁 | teardown 等 handler，handler 等 teardown caller，形成死锁 | public rustdoc 和设计文档明确等待时不能持有 handler 可能获取的锁或阻止 handler 退出的资源锁 |
| T-38 | 全局事件订阅拖慢 IRQ | fanout 后增加无关的全局订阅查询 | timer/IPI/设备 IRQ 在全局锁上竞争 | 已删除通用 IRQ waiter 表及其查询；各设备子系统管理自身就绪通知 |
| T-39 | Hardware NMI acknowledged through the wrong path | Backend misses the special IAR1 INTID or fails to read `ICC_NMIAR1_EL1` | NMI is not acknowledged/dispatched or is treated as an unknown IRQ | Backend classification selects NMIAR1 acknowledgement and generic NMI dispatch, including IRQ-on NMI. Normal-IRQ claims for NMI-owned Lines complete without invoking the handler; NMI registration provides no ordinary-handler fallback. |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 局部影响 | 系统影响 | 检测/缓解 |
|------|----------|----------|----------|-----------|
| F-01 | duplicate regular handler 注册 | 第二个 action 被拒绝 | 设备初始化失败但不会覆盖已有 handler | `register()` 返回 `false` 并 warning |
| F-02 | duplicate shared action token 释放 | token 与目标 line 不匹配或已经释放 | 目标 handler 未移除或释放失败 | `free_irq_action()` 只按 line-local token 删除 action，失败返回 `None` |
| F-03 | unknown IRQ dispatch | 无 handler 可调用 | 中断被 complete，但功能事件丢失 | warning 记录 `Unhandled IRQ` |
| F-04 | NMI action unavailable | No locally published handler | Source event is not serviced | Complete the original claim without invoking a handler or entering a potentially locking NMI logger |
| F-05 | 平台 backend 不 complete 或重复 complete | 控制器状态异常 | 可能中断风暴或中断停止 | claimed IRQ guard 内部 `completed` 标记防止重复 complete |
| F-06 | trap adapter 未禁用抢占 | `handle_irq()` 执行上下文不满足约束 | lifecycle 和调度边界混乱 | `khal::irq` adapter 创建 `NoPreempt` |
| F-07 | deferred executor 递归调用 runner | executor re-enter 自身 | 栈增长或 hardirq tail 不可控 | API 文档禁止递归；softirq runner 用 serving-softirq context gating |
| F-08 | hardirq context guard 未配对退出 | hardirq depth 泄漏 | softirq/BH gating 长期误判 | guard RAII + context diagnostic counter |
| F-09 | softirq handler 泄漏 context state | handler 修改 hardirq/softirq/BH depth 后返回 | 后续 context 判断错误 | handler 前后 snapshot 比较，warning 并恢复 |
| F-10 | `local_bh_disable()` 在 hardirq 中误用 | hardirq 中增加 BH depth | direct runner 被延迟，可能暴露调用者上下文错误 | 记录 `bh_in_hardirq_warnings`，outermost drop 在 hardirq 中不 drain |
| F-10b | BH-disabled raise 被误当作可立即调度 daemon | BH-disabled 临界区中 raise softirq 后抢先唤醒 daemon | 同 CPU direct drain 语义变弱，产生额外 wake 和测试竞态 | `wake_softirqd_if_needed()` 把 BH-disabled 视为 interrupt-like context，只保留 pending；outermost BH enable 或 restart fallback 再处理 |
| F-10c | `raise_softirq_irqoff()` 被 IRQ-enabled caller 误用 | 调用方未满足 `*_irqoff` 前置条件 | `irq_context_snapshot_irqoff()` 读取 per-CPU context state 的前置条件被破坏 | 记录 `irqoff_misuse_warnings` 并在访问 pending/context state 前临时关闭 local IRQ |
| F-11 | descriptor mapping 冲突 | `try_*` API 返回错误或兼容 API warning/fail-fast | 阻止错误 metadata 进入 `IRQ_STATE` | `IrqDescError` 携带冲突字段 |
| F-12 | descriptor 使用未知 domain | `try_*` API 返回 `UnknownDomain` | 设备初始化失败但不会创建不可达 IRQ | domain registry 边界校验先于状态变更 |
| F-13 | controller completion cookie 错配 | complete 错误 active IRQ 或 no-op | 中断丢失、重复触发或 priority/deactivate 状态泄漏 | `PendingIrq` owns the original opaque cookie; generic IRQ core returns only that cookie to the backend. The backend must establish that it represents a valid claim. |
| F-14 | spurious vector 被当作真实 claim | 没有 claim 到中断却调用 generic IRQ/NMI helper | 错误 handler 调用或 completion | irqchip 只在有效 claim 后调用 generic helper；统一 IRQ-exit deferred hook 可以运行，但 `resolved_irq` 保持 `None` |
| F-15 | Reintroducing generic IRQ wake broadcasts | New consumer subscribes to a line rather than its data source | IRQ core acquires unrelated global state and obscures readiness ownership | Consumers register device actions and publish readiness through their own subsystem |
| F-16 | future `WakeThread` 被同步执行 | `WakeThread` return 被当成 handled primary work | future thread 未实现时错误唤醒或执行 sleepable work | 当前 `WakeThread` 分类保持 inert，threaded IRQ milestone 必须显式实现 thread ownership |
| F-17 | action runtime 字段漂移 | `generation`、MSI marker、action list 或 oneshot pending 与 descriptor 不一致 | MSI free 判断、shared/threaded 诊断或 cleanup 决策错误 | descriptor merge/update 统一经过 `IrqStateDesc` 方法，后续新增字段必须同步在该 owner 内维护 |
| F-18 | disable nesting 不配对 | 多次 disable 后只 enable 一次，或 legacy boolean API 语义不清 | IRQ line 长期 masked 或过早 unmask | `disable_depth` 在 `IrqDescRuntimeState` 中显式维护，`enable(spec,false)` 映射 nosync disable，`enable(spec,true)` 映射 depth-aware enable |
| F-19 | Free overlaps dispatch | Another CPU borrows an action or retains claim/replay admission | Caller resources are reclaimed before the handler finishes | `free_irq()` waits for retained-Line admission to drain before node recovery and handler return; it does not delete the descriptor or wait for an action GP |
| F-20 | synchronize_irq 未先 disable | 等待期间设备继续触发新中断 | 调用可能长期等待或只能观察当前窗口 | 文档规定 `synchronize_irq()` 不 mask line；teardown 使用 `disable_irq()` / `free_irq()` |
| F-21 | 释放 shared line 的一个 action 时关闭整条线 | free-by-token 不检查剩余 action | 其它共享设备中断丢失 | `free_irq_action()` 只在最后一个 action 移除后产生 platform disable plan |
| F-22 | enable_irq 作用于未知或无 action descriptor | 显式 enable 创建 descriptor 或 unmask 没有 handler 的 line | 未处理 IRQ 或错误平台状态 | `try_enable_irq()` 不创建 descriptor，未知 IRQ 返回 `UnknownIrq`，无 action 返回 `NoIrqAction` |
| F-23 | enable_irq 与 free_irq wait 重叠 | teardown 已 mask line 但 descriptor 尚未清理 | platform line 在 action 删除后被重新打开 | `teardown_depth` 让 `try_enable_irq()` 返回 `TeardownInProgress` |
| F-24 | Runtime NMI mechanism unavailable | The initialized platform mode is `None`, including unsupported GICv2 configurations | An NMI action cannot obtain local preparation/publication | AArch64 PMU registers an ordinary PerCpu action instead and skips NMI preparation; periodic NMI retirement remains unsupported. This is ordinary registration, not normal-IRQ delivery to an NMI-owned Line, which is rejected without invoking its handler. |
| F-25 | Firmware controller not registered when a consumer maps | Controller driver has not completed initialization, or the wrong node was registered | Mapping fails or a node is attached to the wrong domain | `try_map_fwspec()` returns `ControllerNotReady`; association uses exact `FwNode` identity and registration is explicit after controller init |
| F-26 | Malformed or unsupported firmware binding | Truncated cells, unknown type, out-of-range hwirq | Wrong hardware source configured or silent fallback | `IrqXlateError` distinguishes malformed, unsupported and invalid hwirq; the consumer maps that to `MalformedSpec`/`UnsupportedSpec`/`InvalidHwirq` |
| F-27 | Reusing a mapping with conflicting attributes | Same `(domain, hwirq)` declared with a different trigger or controller | Handler configured for the wrong electrical mode | `try_merge()` returns `TriggerConflict`/`ControllerConflict`; `MappedIrq::desc` never overwrites stored metadata from translation |

## 故障管理

- 注册失败以 `bool` 返回，并尽量在改变状态前检查重复项。
- 查询类 API 使用 `Option` 表达未映射或 strict domain miss。
- Unknown claims still complete. True NMI lookup/action misses do not enter
  the potentially locking diagnostic path.
- handler panic 是不可恢复的内核错误路径，不依赖 claimed IRQ guard Drop 继续
  维持控制器状态。
- platform enable/disable 在需要平台绑定时才调用，plain dynamic virq 不强行触碰
  控制器。
- platform spurious vector 不运行 generic handler fanout；它仍经过统一 IRQ-exit
  deferred hook，与 Linux IRQ entry/exit 配对一致，且 `resolved_irq` 为 `None`。
- context misuse 诊断使用 counters 保留完整次数，warning 输出限流。
- `disable_irq()`、`synchronize_irq()` 和 `free_irq()` 在 interrupt-like context 中通过
  `try_*` API 返回 `InvalidContext`；兼容 wrapper 记录 warning 后返回失败值。
  `disable_irq_nosync()` 是唯一可在 hardirq-like 路径使用的 non-waiting disable API。
- `free_irq()` masks the last non-MSI action's source and waits for
  in-flight claims to finish before node recovery. All Lines remain owned
  independently of their actions,
  including domainless descriptors. During the wait,
  `teardown_depth` rejects registration and enable. `unregister()` is a
  compatibility wrapper. Waiters must not hold locks needed by the handler.
- MSI descriptors bypass ordinary platform configure/enable. Allocation failure
  returns `None`; composition/publication failure explicitly returns the private
  reservation to the backend. `free_msix()` validates unused state and reservation
  ownership before withdrawing both owning indexes. RCU retirement submission
  precedes backend consumption of the reservation. Backend release
  has no recoverable failure after withdrawal. NMI calls are rejected before locks.
- lifecycle hook 注册是单 owner；后续如果需要多个 consumer，应显式升级为
  notifier chain，而不是让多个子系统覆盖同一全局 hook。
- No global IRQ waiter table is consulted after action fanout. Descriptor-local
  in-flight completion is published under the retained Line lock and woken
  outside it. This path does not acquire IRQ_STATE or remove a descriptor,
  including an empty domainless Line prepared before handler installation.
  Explicit MSI release still checks aggregate-idle execution before directory
  withdrawal; action removal never disposes a descriptor.
- deferred executor 注册是单 owner。`softirq::init()` 在启动路径安装默认 owner；
  重复注册返回 `false`，空 executor 也返回 `false`；runner 在未注册时返回
  `NoExecutor`。
  Registration and execution handoff are crate-internal. Clearing is
  `cfg(unittest)` only; hook-record mutation and atomic publication share the
  registration lock. A reader may already have loaded a callback, so test
  clearing is not a callback-drain or production unload guarantee. Fixtures
  restore the boot owner rather than leaving device deferred work stranded.
- `kworkng` publishes per-CPU normal/BH services only after preparation and
  action registration. `kworkqueue_ng` owns queue/work state;
  `kworkerpool_ng` owns normal managers and workers. `kirq` supplies context
  predicates and BH dispatch, not work storage or scheduler accounting.
- Synchronous work waits require sleepable context. Before freeing device state,
  callers must stop producers and drain owned delayed timers and work. Queue
  closure alone is not timer shutdown. See `task/kworkng/docs/security.md` for
  safe-handle deferred reclamation and borrowed-storage obligations.

## 隐私分析

`kirq` 不处理用户数据。它处理的信息主要是 IRQ number、controller metadata、
handler 引用和 CPU targeting policy。日志可能暴露 IRQ 编号或 hwirq，对普通用户
可见前应由 procfs/syslog 权限策略控制。

## 已知限制

- Descriptor-backed unclaimed action fanout uses saturating event counters
  and power-of-two warning sampling. Accounting is serialized by the existing
  Line lock used for in-flight completion; formatting and logging occur after
  releasing that lock. No directory lookup or lock is needed for accounting.
  It never masks an IRQ or changes disable depth. Any visited shared action claiming
  an IRQ prevents an unhandled count, including a faulty always-claiming
  action; these diagnostics cannot establish that hardware was acknowledged.
- Unhandled reports are bounded per descriptor lifetime, not per unit of
  time or across repeated descriptor recreation. Counters combine CPUs and
  action-set changes within that lifetime. A suppressed tail has no public
  query or teardown report and is discarded with the descriptor. Unknown-source,
  missing-descriptor, no-action admission and unsupported-thread warnings remain
  separate, so this change does not contain all IRQ-related log storms. A
  traversal emptied by concurrent unlink merges no claim and is counted as
  unhandled; it does not promise exact admission-time membership.
- Ordinary shared actions use a private node chain bounded by allocation availability and
  checked publication-sequence/count arithmetic, without a fixed fanout cap,
  per-action statistics, procfs output, debug-shirq injection or a Linux-style
  dev_id ABI.
- MSI/MSI-X 当前只提供最小 allocation/message bridge，不支持 interrupt remapping、
  managed IRQ、affinity rebalance 或动态 post-enable MSI-X table entry 管理。
- lifecycle hook 目前是单 owner 模型，不是 notifier chain。
- The deferred executor is softirq's single-owner hardirq-exit handoff, not
  a workqueue or IRQ thread. Workqueues use explicit publication through
  `kworkng::DefaultQueue::Normal`.
- `/proc/interrupts` 仍无 per-IRQ/per-CPU 统计输出。
- Readiness wakeup belongs to the device subsystem, not generic IRQ dispatch.
- `PER_CPU` is metadata, not a per-CPU enabled set or a teardown barrier. Resolved
  PerCpu sources use non-nesting CPU-local enable state, including through the retained
  legacy boolean enable bridge. Before freeing
  shared handler resources, callers need a separate proven local shutdown on
  every relevant CPU, protected against re-enable. Remote IPI submission and
  original-Line software drain do not supply that proof. The current boot-only
  activation paths must not be interpreted as a runtime shutdown protocol; see
  the [per-CPU lifecycle gate](design.md#per-cpu-source-lifecycle).
- `synchronize_irq()` uses descriptor-local completion and `TaskCompletionWaitIf`
  to wait for aggregate idle. Completion is only a wake source: recheck
  `!is_running` under the original Line lock after waking. PerCpu is rejected.
  Future threaded execution must define its ownership within this boundary;
  existing context restrictions still apply.
- Synchronous waits and dispatch guards retain the original `Arc<IrqLine>` and
  update only its locked state. Number reuse or directory withdrawal cannot
  redirect them to a replacement. Cleanup compares Line identity before removing
  a directory entry. Ordinary removal requires idle running state; PerCpu removal
  requires completed source-owner exclusion before the unsafe call;
  retaining storage neither grants management admission nor establishes hardware
  quiescence. Management transactions acquire management exclusion before
  registry and Line locks. NMI publication uses the same order, without global
  table/publication locks. Wait/wake callbacks run outside all short locks.
- Ordinary running admission covers its required controller completion/tail work,
  not merely the handler snapshot. Empty Level/Edge/Fasteoi arrivals instead
  complete under the held Line lock without a credit, as do rejected normal-IRQ
  claims for NMI-owned Lines and rejected PerCpu claims. Accepted
  PerCpu executions rely on source-owner exclusion until full dispatch return.
  Ordinary drain and resource
  release check state under that same lock, so they cannot pass a locked rejected
  completion already in progress. Lookup misses still
  complete their original claim without acquiring a later Line by number.
  The private consuming `IrqDispatch::complete` centralizes this normal-return
  ordering; it is not a type-level ban on dropping an admission early or a
  recovery guarantee after a kernel panic. Synthetic completion tests exercise
  software ordering and retained identity, not real controller delivery.
  This is a software drain boundary, not hardware pending-state quiescence.
  Selected Fasteoi Lines use running ownership for admitted software replay,
  while pending queue entries own no execution admission.
  Supported providers and the outstanding flow
  prerequisites are enumerated in [Controller Flow Contracts](design.md#controller-flow-contracts).
- Disable releases registry exclusion before calling the controller, but keeps
  the original Line state locked through the mask update, without acquiring the
  global control lock. The backend must be bounded, non-sleeping and must not
  reenter KIRQ registry/control/dispatch.
  An outer local IRQ/preemption guard preserves the entry mask across the
  non-LIFO registry-to-Line handoff; removing global mutual exclusion must not
  remove this local context protection.
  Controller-register locks are below Line state in this path; providers must
  release those locks before calling a generic dispatch entry. Drain waiting
  and the retained Line's final release happen outside all short locks.
- Enable retains the selected Line before releasing registry exclusion and
  keeps its state locked through configuration and unmask. Both controller
  operations obey the same non-reentrancy and bounded-execution contract as
  mask. Both normal and legacy enable reject a missing descriptor before
  allocation, mapping publication or controller application. A surviving
  reverse mapping alone cannot authorize reconstructing its missing Line.
  Legacy force-enable may merge compatible metadata into an existing Line only
  while its binding is not in use; otherwise descriptor and flow must remain
  unchanged. It does not replace explicit descriptor preparation. Final owner
  release stays outside short locks.
  All registration paths, including explicit startup variants, reject missing descriptors before
  preflight, configuration or action publication. Rejection retains no handler
  owner and does not create a reverse mapping. Neither mode implicitly allocates
  a descriptor. An existing identity is revalidated
  after management acquisition. NMI source shutdown remains caller-owned;
  old handler borrows and claims must finish before successful local teardown
  revokes publication; action removal performs no RCU wait.
  The outer local NoPreemptIrqSave guard masks IRQs before registry and Line acquisition
  and restores context only after both guards are gone. It protects the non-LIFO
  registry-to-Line handoff independently of cross-CPU mutual exclusion.
- Final enable admission checks teardown under the same Line guard as disable
  depth changes and controller application. A stale earlier observation cannot
  authorize enabling a tearing-down Line. Legacy enable checks teardown before
  merging metadata and retains that guard through resolution and application.
  Resolution validates identity, descriptor compatibility and mapping conflicts
  before mutation. Rejection does not publish metadata or mappings, although
  selecting a fresh dynamic identity may advance the allocator.
- Regular/shared registration relies on management admission for membership
  stability, not a directory lock around the controller call. Registration
  preflight, metadata resolution, installation and configuration/unmask share
  one Line guard. Callbacks borrow the locked state and must not reacquire
  registry/publication/Line locks or wait. The caller retains
  the handler across the entire transaction so rejected or discarded cloned
  owners cannot run final client destructors under Line or management locks.
- Ordinary Level/Edge/Fasteoi action withdrawal validates sleepable context before unlinking, retains
  the detached node through last-action masking, then releases Line exclusion.
  It drains original claim/replay obligations before recovering the node and
  returns handler ownership only after management cleanup. No action retirement
  allocation or GP preparation occurs. The now-unproducible public
  `IrqDescError::RetirementUnavailable` variant was deliberately removed, not
  repurposed for another failure. Index retirement retains its separate failure
  rules and is not an action-lifetime prerequisite.
  Only last-action withdrawal adds whole-Line teardown. Non-last shared removal
  leaves survivors eligible under normal flow rules throughout the drain;
  continued survivor traffic may prolong the Line-wide drain. Membership remains
  serialized by the management mutex. `IrqDrain` and source-owner shutdown
  obligations remain in place; execution drain is not hardware shutdown.
  Shared ownership and rejection of ordinary removal for NMI-owned Lines are unchanged.
  An absent action or unmatched token cannot change the Line's enable state,
  disable depth or teardown state, or invoke a controller plan. Successful
  action removal is required before the last-action masking transaction.
- `runtime/action_set.rs` is the ordinary membership/count/token audit surface.
  Its writer holds the Line lock inside management serialization. Unlink preserves
  detached next links until ordinary execution drain. PerCpu removal instead
  requires every admitted dispatch to have returned fully before entry.
  A failed removal leaves membership
  unchanged. Publication sequences and shared keys never wrap or get reused.
  A snapshot borrows the dispatch guard, not a list Arc or individual handler
  owners. Acquire traversal pairs with Release publication and invokes callbacks
  without short locks. Detached nodes are recovered in the removing task after
  ordinary drain or prior PerCpu source-owner quiescence, never by an asynchronous
  destruction callback.
  Concurrent unlink can bypass an unseen action; exact admission-time membership
  is not guaranteed. A later append has a sequence above the captured ceiling
  and cannot extend that pass. Edge replay captures a fresh ceiling each pass.
  NMI ownership remains separate and contributes only its owner flag to the count.
- NMI management releases the directory before controller configure/mask,
  retaining the original Line lock through the bounded operation. It releases
  all guards before final owner destruction, without an action GP wait.
  Source shutdown and completed CPU-local teardown are prerequisites, not
  consequences of owner withdrawal.
- `flush_work()` / `cancel_work_sync()` cover ordinary work items. The legacy
  delayed-work product API and timer adapter have been removed. Automatic delayed
  retry, full Linux concurrent-flusher handoff, CPU hotplug drain and rescuer
  semantics remain unsupported.
- `WakeThread`、`IrqThreadSlot` 和 oneshot pending 只是 foundation state。它们尚未绑定
  scheduler task、workerqueue、mask protocol、thread teardown、CPU affinity 或
  per-action statistics；带 future-only thread slot 的 action 在当前 core 中不可
  dispatch，不能降级为 hardirq 同步 primary。

## 审计清单

- 新增 IRQ API 是否明确说明 hardirq/NMI/普通上下文约束。
- 新增 dispatch 路径是否在调用 handler 前释放 `IRQ_STATE`。
- 新增 action return 分类是否保持 `NOT_HANDLED` 为 unhandled，而不是 threaded wake。
- New `WakeThread` handling must have independently defined IRQ-thread ownership.
- 新增 threaded IRQ 设计是否先定义 thread slot ownership、cancel/teardown ordering、
  oneshot mask/unmask、CPU affinity 和 sleepable context 边界。
- 新增 shared IRQ 设计是否在 `IrqDescRuntimeState` 内维护 action list、dev-id identity
  和 fanout result aggregation，而不是只增加 flag 或计数。
- New wake notification must follow device-owned readiness rather than restore
  generic IRQ line/source subscriptions.
- 新增 NMI 功能是否避免读取或写入 normal IRQ shared state。
- 新增平台 backend 是否满足 `PendingIrq` completion 和 `notify_cpu`
  publish-before-notify 契约。
- 新增平台 backend 是否只在 generic handler fanout 后对 level IRQ 做最终
  EOI/deactivate。
- 新增 context check 是否把 BH-disabled 视为 non-sleepable，而不是普通 task context。
- 新增 diagnostics 是否保留计数、避免分配，并且不会在 hot path 里无限刷 warning。
- 新增 MSI backend 是否只暴露 virq/message，且不让驱动读取 APIC id 或裸 CPU vector。
- 新增 softirq deferred execution 功能是否通过 `bottom_half/deferred.rs` 的
  hardirq-exit handoff 进入；独立 irq_work 则必须由普通 IRQ 分发，不能在 NMI exit 消费。
- deferred executor 是否在 completion 之后、lifecycle exit 之前运行。
- deferred executor 是否仍保持单 owner 且不引入 `ktask`/`kprocess` 依赖。
## Management Mutex Contract

The management guard is held directly by KIRQ and is not Send; it must not
be unlocked by a different task. NMI is rejected before ordinary task-context
queries; waiting acquisition rejects every context rejected by the scheduler's
sleepability predicate. Startup try-acquisition never parks, including when
running on the initialized idle task. It is not permission to sleep inside a
masked or otherwise non-sleepable transaction. Preparing a Line must happen
outside short locks, and an unlocked mutex's final destruction must not wait.

These checks do not validate IRQ directory membership, hardware quiescence or
application dependency cycles. KIRQ revalidates retained Line identity after
acquisition and records admission before releasing the registry lock; explicit
resource release excludes admitted Lines. A stale remover cannot touch a
replacement. Ordinary admission completion releases only the membership pin,
regardless of the returned value. Only an explicit successful action-removal
completion releases admission and its teardown credit together under the
original Line lock. Error and rejected-removal paths cannot infer retirement
from a general transaction result. Only release of a teardown credit can
trigger flow restoration; non-last shared removal must not unmask over a running
survivor's flow. It never looks up or
withdraws a descriptor, and therefore needs no registry lock or non-LIFO guard
handoff. The caller retains the Line owner and management mutex until completion
returns. Handler owners are returned outside locks for final destruction.
Registration may retry, but only after releasing the old mutex. Waiting for
dispatch holds management exclusion, not publication/registry/Line spinlocks.
Caller-owned handler dependencies must not require the same management mutex
while a removal waits for that handler; serialization cannot resolve such a
dependency cycle.

## Rejected Management Transactions

Ordinary action admission is checked before merging caller-supplied descriptor
metadata or publishing domain mappings. Preflight and installation reuse the
same ownership, capacity and token predicates. The retained Line management
mutex and continuous Line state exclusion serialize both phases; registry
exclusion covers identity revalidation and metadata publication. Caller references keep rejected
handlers alive until all management and short locks have been released. NMI
context rejection does not invoke the locking logger.

Resolution-error cleanup releases admission and management exclusion without
retiring an existing idle Line. A free request that finds no owned action
returns before changing enable/depth state, masking hardware or beginning
teardown/drain. These guarantees cover software rejection and absence, not
rollback of hardware writes, allocator identity reuse or device quiescence.

## Normal Dispatch Tail Ownership

The private `IrqDispatch::complete` consumes the dispatch admission only after
the supplied original-claim completion returns. `runtime/entry.rs` moves that claim
into a once-only closure on the claiming CPU. Missing Lines still complete their
claim without attaching to a replacement. For selected Fasteoi Lines, only the
dispatch with running ownership clears it, under the original Line lock held
through completion. Level/Edge/Fasteoi/PerCpu rejections hold Line exclusion continuously
through chip operations and original completion, take no in-flight credit and
never clear another owner's running bit. Accepted ordinary handlers and replay
take running ownership before snapshot capture and unlock.
Rejected normal-IRQ claims for NMI-owned Lines complete under the held
Line lock without a count or action access. Accepted PerCpu executions rely on
source-owner lifetime protection until full dispatch return;
PerCpu rejection has no count and cannot borrow an action.
The drain's aggregate-idle observation takes the same Line lock: rejected
completion is serialized by that lock, while running ownership protects ordinary
executions that release it. Running ownership may be cleared transiently
for the final chip/completion transaction only under continuous Line exclusion,
after snapshots are consumed; drain cannot observe that intermediate state.
Ordinary running admission resets `execution_idle` only on idle-to-busy;
waiters obtain it through `execution_idle_completion()`.
Finishing running admission publishes completion when running ownership is
absent. Ordinary tails mark the guard Finished before unlocking, preventing a
second finish in Drop; deferred wakes run outside the lock. A stale wake cannot
mark a newly reinitialized completion done.
Removing rejection credits therefore does not let sync/free skip an ongoing
locked completion. Neither mechanism excludes future arrivals or stops a source.
Registry exclusion is released before hardware operations; the Line guard must
be released before admission completion reacquires the original Line. Handlers
never execute under these short locks. This does not establish hardware
quiescence or type-level prevention of dropping an internal admission early.

## Fasteoi Replay Ownership And Progress

Resolution rejects flow changes while an action, aggregate execution or queued
replay flag remains.
For Fasteoi, it also rejects source-description changes during that interval,
even after the last action is withdrawn. These checks precede mapping and
descriptor mutation, preserving the original operation table, generation and
source for admitted completion/replay. Private-registry regressions cover both
ownership gates; they do not prove hardware quiescence or affinity migration.

Only an explicitly software-capable chip may use BH replay. A hardware-only
retrigger executes under the original Line lock with local IRQs excluded and
must be bounded, non-sleeping, allocation-free and non-reentrant into KIRQ.
Success means the controller accepted a pending request, not that a handler
ran or hardware became inactive. Failure leaves pending intact, without a
software fallback or an automatic progress guarantee. Hardware acceptance
does not retain a software queue owner or add an in-flight credit; a later
real claim acquires its own admission and completion obligation. Callers still
must stop hardware sources before releasing device resources. Ordinary GIC SPI
providers select this capability and accept the pending write without readback;
this is not proof of a subsequent delivery. GICv3 mask timeout is diagnosed but
the infallible mask hook cannot establish hardware quiescence on timeout.

Flow suppression must not be coupled to the presence of a new management enable
write. `IrqPlatformPlan::flow_unmask` is selected independently using the plan's
proposed enabled/depth state and current action/teardown ownership, and applied
after configure and optional enable while Line state remains locked.
Preparation changes no runtime control fields; only the post-apply commit
updates configured generation, enabled state and disable depth. The same Line
exclusion must cover preparation, hardware application and commit without an
intervening descriptor/control-state change. Other CPUs may receive a source
enabled by the backend, but their generic admission cannot observe this Line
until the commit has completed and its lock is released. True NMI readers
instead observe only CPU-local action publication after preparation; source
owners must not activate delivery
before preparation. Neither path establishes hardware rollback on failure. This restores a
previously enabled, action-less Line
when its first action is installed even with `enable: None`, without reopening
disabled or teardown-blocked Lines. Pending scheduling follows the transaction;
source unmask and software replay remain separate obligations.
The [configuration failure gate](design.md#ordinary-flow-and-replay)
records the retained mapping mutation that precedes the fallible hook. Regular/shared
configuration now precedes acquisition of the new action owner; NMI configuration
precedes publication of the sole Line-owned action. Delivery restoration remains after
action installation, under continuous Line exclusion. Existing shared owners
and disable depth remain intact during the configuration-only stage.
Disabled regular registration also configures before publishing its owner, but
does not enable or flow-unmask the source. Configuration readiness does not
grant delivery permission; the explicit enable transaction releases that gate.
Configuration errors propagate before new action publication and prepared
control-state commit. `runtime.is_configuration_failed` starts false and latches
only configuration errors; metadata-only merges do not latch failure. Configured
generation, enabled state and disable depth remain unchanged. The latch gates
`can_run_actions`, so queued replay declines execution without admission or chip access
without consuming pending, and withdrawal cleanup cannot unmask the source.
Already running handlers are not cancelled by this gate.

Recovery forces configuration even with a matching generation; successful
configuration commit clears the latch. Enable recovery forces a hardware enable
write despite an already true logical enabled state. Shared registration saves
restoration eligibility before configuration and restores hardware enable after
owner publication only when logically enabled with zero disable depth. NMI
configuration errors latch failure and successful configuration commit clears
it before publishing the Line-owned NMI action, without replacing source shutdown.
The failure gate and recovery ordering are implemented; validation remains
pending. Neither this software gate nor successful plan ordering proves hardware
rollback or quiescence.

The static chip capability supplies code lifetime, not permission to rebind a
live Line. Selection validates PLIC and ordinary GIC SPI identity and capability;
fixed local sources select PerCpu explicitly. Unknown or foreign sources are
rejected with UnsupportedFlow, not admitted through a fallback flow.
Binding changes
are checked before publication while actions, aggregate execution or the explicit
queued flag retain the binding.

Replay insertion retains the original Arc and marks queue membership under Line
exclusion, without execution admission or an in-flight increment. Queue operations use Line then queue lock;
consumers drop the queue lock before acquiring Line. Notification runs outside
both. The queued Arc prevents numeric reuse from redirecting work and retains
Line storage until the consumer releases it. Queue membership deduplicates submissions
and explicitly blocks incompatible flow/source changes;
running ownership prevents overlapping action execution on the selected flow.

`is_replaying` distinguishes software service from an outstanding hardware
claim. A real claim overlapping software service retains pending before its
completion, preventing loss after the replay handler's final device read.
The owning replay clears both flags, completes its claim (a no-op for software
service), and prepares pending service under the same Line lock. Fasteoi hardware
completion uses the same ordering. Only normal enable/teardown eligibility permits
retrigger or queue insertion. The returned permanent executor notifier runs after
unlock; withdrawal may win before notification, so dequeue must still revalidate
eligibility. `resend::prepare_locked` requires the supplied state to belong to the
retained Line; its caller locks that exact Line directly.

The executor clears queue membership and checks retirement, pending,
enabled/depth, configuration, action presence, teardown and running state under
the Line lock. Only an eligible execution calls `begin_running_dispatch` before taking
a fresh action snapshot under that lock. Rejected retired, disabled or action-less
entries access neither handlers nor chip operations. Before enqueueing,
those eligibility conditions must hold before explicit LevelHigh/LevelLow may
clear pending in reliance on the declared hardware level contract. Unknown
trigger never acquires that exemption merely from PLIC/Fasteoi classification.
Incorrect level metadata can therefore lose software service; a state-only
fixture is not evidence of physical reassertion. Withdrawal invalidates
eligibility rather than physically unlinking queued nodes. Sync/free drains
admitted claims and running replay only; it may return before a pending executor
runs. That executor holds no old cursor and cannot snapshot a removed action.
Pending service is Line-scoped, not tied to an action-registration epoch: later
registration on the same live Line may make a new action eligible for that replay.
This does not authorize the removed owner to be borrowed again. KIRQ keeps lazy
queue removal; unlike Linux's `clear_irq_resend`, it does not eagerly remove a
still-queued entry. Neither policy can cancel an entry already popped for execution.
Deferred wakes and final
handler/Line owner releases must remain outside short locks. Software replay
does not construct PendingIrq or complete the old hardware cookie.

Non-last shared removal must neither stop surviving actions nor interfere with
their flow-owned mask interval. It adds no whole-Line teardown credit, so
management completion cannot use that removal as a reason to unmask. For a
transaction that did gate the Line, completion first releases its teardown
credit and checks `can_run_actions` before flow restoration under the original
Line lock. No directory lock or additional context guard is acquired. Pending
scheduling follows after unlocking and uses current eligibility, not a saved
pre-withdrawal enable flag. Last-action removal, disable depth or remaining
teardown prevents reopening. NMI teardown retains its separate existing protocol.

`resend::install` is unsafe because its permanent notifier must be IRQ-safe,
allocation-free, non-inline and capable of scheduling the BH runner without
cancellation or shutdown. `run` requires softirq context and rejects NMI; replay
itself excludes local IRQs/preemption. No sleepable management lock is acquired
by replay. Client handlers must support non-sleeping BH replay and must not wait
for management exclusion held by their own synchronous withdrawal.

Before executor binding, queued Arcs and membership flags remain live, without
in-flight obligations. Installation notifies the backlog. Pending service and
eventual queued-owner release require executor progress, but an aggregate-idle
sync/free does not. Already admitted executions must remain able to finish while
the caller waits. There is no eager queue cancellation. The retained Arc protects
descriptor storage, not device resources or permission to access a retired chip
binding; source owners still stop producers and exclude restart before resource
release. Current runtime installs work services before external device/console
IRQ setup. Hardware-active queries, device
shutdown, dynamic affinity and CPU hotplug remain separate proof obligations.
Neither synthetic fixtures nor this documentation establish physical delivery.

## Edge Delivery Boundary

`backend/platform.rs::EdgeChip` is a trusted controller contract. Its callbacks
run under the original Line lock with IRQ/preemption exclusion and must not
sleep, allocate, reenter KIRQ or wait for remote handler completion. The lock
order remains registry -> Line -> controller registers/allocation -> local APIC.
`runtime/dispatch.rs::run_edge` retains admission across all coalesced action
iterations; it never reuses an acknowledged claim or clears a concurrent owner's
running state. Disabled/tearing-down/failed-configuration Lines retain pending
without executing handlers. A failed retrigger retains pending without silently
falling back to another CPU or a software callback.

`FasteoiResend::Level` requires a provider-validated level source, not a guess
from an unspecified firmware trigger. It relies on hardware reassertion after
unmask and does not prove physical drain. MSI's device-owned mask and outstanding
APIC delivery remain owner shutdown obligations. Neither aggregate idle nor a
successful retrigger is a vector-reuse barrier; the existing MSI release API
does not gain a new shutdown guarantee from retained target metadata.

MSI release holds the registry lock across unused/ownership checks and both
index withdrawals. It restores task context and submits RCU retirement before
backend release, which provides its own allocator exclusion. A private
non-cloneable `MsiReservation` and both preallocated index retirement records
are attached before the Line is published; an MSI flag or raw IRQ number alone
cannot authorize release. The backend consumes
the reservation and must release it without sleeping or reentering KIRQ. It
cannot report an ordinary failure after withdrawal. Software rejection preserves the
mapping and its retirement records; callers must still retain a usable allocation
identity. Release no longer adds an OOM failure to otherwise valid cleanup.
This does not repair an adapter that consumes its retry handle on rejection. Private-state
release tests check software failure atomicity, not device shutdown or physical
late-message isolation.

Allocation rollback calls the backend without the registry lock using the same
fresh unpublished reservation returned by failed publication. Backend allocator
exclusion is required in both call contexts. The x86 release has no fallible
hardware work: an out-of-range/already-free owned vector indicates a violated
allocation invariant. A backend with recoverable release failures requires a
pre-withdrawal protocol before integration. Reservation Drop does not release
hardware, and final Line destruction occurs outside the registry lock.

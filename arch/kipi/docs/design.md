# kipi — 设计文档

## 定位

`kipi` 是 X-Kernel 的跨 CPU 协调模块，向上提供：

- 通用 IPI 回调投递与广播；
- I-cache 跨核失效的实现绑定；
- TLB shootdown 的跨核通知与确认协议；
- 进入平台电源终点前的系统级 CPU 停机协议（`khal::power::SmpStopIf`
  的 provider，`stop_other_cpus`）。

它位于 `kirq::notify_cpu()` 之上、调度器和页表实现之旁，为内核中
“需要让别的 CPU 立刻执行一小段本地操作”的场景提供统一入口。

## 背景

内核中有两类典型的跨核需求：

1. 让指定 CPU 或一组 CPU 在中断上下文中执行一个短回调，例如 I-cache flush。
2. 在页表修改后通知其它 CPU 丢弃本地 TLB 项，并等待确认，避免旧映射继续生效。

这两类需求都依赖 IPI，但约束不同：

- 通用回调更关注投递、排队和广播语义；
- TLB shootdown 更关注发布顺序、确认协议、并发安全和等待行为。

`kipi` 将它们放在同一 crate 中，复用同一 IPI 向量，但把协议状态分层管理。

## 范围

涉及的源文件：

```text
arch/kipi/
├── src/
│   ├── lib.rs
│   ├── event.rs
│   ├── queue.rs
│   ├── icache.rs
│   ├── stop.rs
│   └── tlb.rs
└── Cargo.toml
```

其中：

- `lib.rs`：对外 API、每 CPU 事件队列、IPI handler；
- `event.rs`：单播/广播回调模型；
- `queue.rs`：每 CPU FIFO 队列；
- `icache.rs`：I-cache 跨核 flush 的 crate interface 实现；
- `stop.rs`：系统级 CPU 停机协议（电源终点前停靠其它 CPU）；
- `tlb.rs`：TLB shootdown 发布、IPI 发送、确认和等待协议。

## 架构

`kipi` 把“通用事件投递”和“TLB shootdown 协议”叠加在同一个 IPI handler 中：

```text
发起 CPU
   │
   ├─ run_on_cpu / run_on_each_cpu
   │      │
   │      ├─ 向目标 CPU 的 IPI_EVENT_QUEUE 入队回调
   │      └─ kirq::notify_cpu(kbuild_config::IPI_IRQ, ...)
   │
   └─ tlb::flush_remote
          │
          ├─ 发布每发起 CPU request slot
          ├─ bump 每目标 CPU pending epoch
          └─ kirq::notify_cpu(kbuild_config::IPI_IRQ, ...)
                     │
                     v
目标 CPU IPI handler
   │
   ├─ tlb::handle_shootdown()
   │      ├─ fast path: 检查 pending epoch
   │      └─ slow path: 扫描 request slots，flush 并 ack
   │
   └─ 处理 IPI_EVENT_QUEUE 中的普通回调
```

这里有两个核心状态面：

- **每 CPU 事件队列**：存放通用回调，FIFO 执行；
- **每发起 CPU request slot**：存放 TLB shootdown 请求；
- **每目标 CPU pending epoch**：为 TLB handler 提供 O(1) fast path gate。

## 调用约束 / 执行上下文

### 通用 API

- `init()` 必须在对应 CPU 上初始化本地 IPI 队列后才能接收远程投递。
- `run_on_cpu()` / `run_on_each_cpu()` / `run_on_each_cpu_via_ipi()` 只能在
  `kirq::notify_cpu()` 可用后使用。
- 回调必须足够短，不应依赖长时间阻塞语义；它们最终在 IPI handler 上执行。

### 执行上下文

- `ipi_handler()` 运行在中断上下文，不可睡眠。
- `run_on_each_cpu()` 在当前 CPU 上同步执行一次广播回调，在其它 CPU 上通过
  IPI 上下文执行。
- `run_on_each_cpu_via_ipi()` 则连当前 CPU 也走队列和 IPI handler 路径。

### TLB shootdown 约束

- TLB shootdown 依赖所有 AP 已经启动，`mark_all_cpus_started()` 之后才会真正发 IPI。
- `flush_remote()` 在发布 request slot 的生命周期内禁用抢占，防止同一 CPU 上的
  另一个任务复用相同 slot。
- 用户页表 shootdown 的目标集合由调度路径维护的 mm-owned residency 提供；
  `flush_remote()` 只消费快照，不在 flush 结束后重建或 reset residency。

## 状态机

### IPI 事件队列

通用回调没有复杂状态机，单个事件生命周期为：

```text
创建 Callback / MulticastCallback
    -> 入目标 CPU 队列
    -> 发送 IPI
    -> 目标 CPU pop_one()
    -> 执行回调
    -> 销毁
```

### TLB shootdown request slot

每个发起 CPU 持有一个 request slot，生命周期为：

```text
inactive
   -> try_activate()
   -> clear_targets / mark_target
   -> publish(request_seq, vaddr/flush_all)
   -> bump 目标 CPU pending epoch
   -> send IPI
   -> wait for per-target ack（等待中轮询 `handle_shootdown`，IRQ 关闭时也能 ack 对方）
   -> deactivate()
```

如果同一 CPU 在 slot active 期间再次进入 `flush_remote()`，不会立刻重入复用，
而是设置 `needs_retry_full_flush`，由外层在本次请求结束后补发一次全局 retry。

## 算法流程

### `run_on_cpu`

1. 校验目标 `LogicalCpuId` 是否在配置范围内且存在于逻辑 CPU 映射中。
2. 若目标是当前 CPU，直接本地执行回调。
3. 若目标是远程 CPU，检查其 IPI 队列是否已初始化。
4. 将回调压入目标 CPU 的 `IPI_EVENT_QUEUE`。
5. 调用 `kirq::notify_cpu(kbuild_config::IPI_IRQ, Specific(...))` 发 IPI。

### `run_on_each_cpu`

1. 校验所有远程 CPU 队列已准备好。
2. 当前 CPU 同步执行一次回调。
3. 其余 CPU 入队单播化后的回调。
4. 通过 `AllButSelf` 发广播 IPI。

### `ipi_handler`

1. 先调用 `tlb::handle_shootdown()`。
2. 再循环消费本 CPU 队列中的普通 IPI 回调。

这个顺序是刻意设计的：TLB shootdown 属于页表一致性协议，优先级高于普通跨核回调。

### `tlb::flush_remote`

1. 获取当前 CPU 的 active request slot，并禁用抢占。
2. 根据目标 `KCpuMask` 生成不含自己的目标数组。
3. 分配新的 `request_seq`。
4. 标记目标 CPU，发布 `(request_seq, vaddr | flush_all)`。
5. 为每个目标 CPU bump `pending epoch`。
6. 发送 IPI。
7. 等待每个目标 CPU 将 `acked_seq_by_cpu[target]` 推进到该 `request_seq`。
   等待循环里轮询 `handle_shootdown()`：页表路径常在 IRQ 关闭下走到
   `flush_remote`，若只等 IPI handler，两核互等会谁也 ack 不了。
8. 释放 active slot；若期间检测到同 CPU 嵌套请求，则补发 full retry flush。

### `tlb::handle_shootdown`

1. 读取当前 CPU 的 `pending_epoch`，与本 CPU `LAST_HANDLED_PENDING_EPOCH` 比较。
2. 若未变化，直接返回，不扫描所有 slots。
3. 若有变化，遍历所有发起 CPU 的 request slots。
4. 对每个 slot：
   - `Acquire` 读取 `published_seq`；
   - 若该 request 目标包含当前 CPU 且尚未 ack，则执行本地 `flush_tlb(...)`；
   - 更新对该发起 CPU 的 ack。
5. 把本次入口时看到的 `pending_epoch` 记为已处理值。

### `stop_other_cpus`

本模块以 `#[kiface::provide]` 实现 `khal::power::SmpStopIf`（UP 构建不
链接本模块，由 `khal` 内的 no-op 兜底）；本地 IPI 事件队列就绪之前
（即 `kipi::init()` 之前）调用退化为 no-op，终点直接落到裸平台终点。

系统级 CPU 停机采用**标志位协议**而非回调队列，保证不分配内存，且即使
目标 CPU 的 IPI 事件队列已满也能送达：

1. orchestrator 用一次 `compare_exchange(NO_STOP_REQUESTED, 自身逻辑
   CPU id)` 在 `STOP_STATE` 上**同时完成选主与发布**：该原子值既是停机
   请求标志又是 orchestrator id，不存在"请求已可见但 id 未知"的中间状
   态；并发调用方 CAS 失败，置 ack 并自行 park。发布后向其它所有 CPU
   发一个裸 IPI。
2. 任何 CPU 进入 `ipi_handler` 时，若看到停机请求且自己不是 orchestrator，
   先完成所有仍会触碰共享状态的收尾（日志、屏蔽本地中断、停靠本地
   NMI/pseudo-NMI 源，`khal::quiesce_nmi`），**然后才**置位本 CPU 的
   `STOP_ACKED`——ack 是停靠 CPU 对共享状态的最后一次写，之后立即进入
   屏蔽本地异常的 `wfi/hlt` 永久停靠。orchestrator 观察到 ack 即可确定
   该 CPU 不会再执行任何共享状态路径。
3. orchestrator 以 1s 有界超时轮询等待集中各 CPU 的 ack。等待集在发布时
   一次性快照：只包含 IPI 队列已初始化的 present CPU（见设计决策）。
   超时仍未 ack 的 CPU 不阻塞终点，逐个告警后继续，语义对齐 Linux
   `smp_send_stop()`。

orchestrator 全程持有 `NoPreempt` 守卫钉在本 CPU 上（见设计决策），否则
认领、广播与等待所依据的 CPU id 会在迁移后失效。

停靠前必须停靠本地 NMI 源：CPU 级异常屏蔽挡不住真正的 NMI（如 x86 NMI
看门狗）；若不停靠，停靠中的 CPU 会被看门狗 NMI 反复唤醒，并把“故意停机”
误判为 hard lockup，panic 后走平台断电，从而破坏 `khal::power::halt()`
的语义。

## 并发模型

### 通用 IPI 队列

- 队列是 **每 CPU 独占消费、远程生产** 模型。
- 远程 CPU 通过 `remote_ref_raw(...).lock()` 入队。
- 当前 CPU 只在本地 IPI handler 中 `pop_one()`。
- 队列锁使用 `SpinNoIrq`，因为会被 IPI handler 触达。

### TLB request slot

- 每个发起 CPU 一个 slot，避免多个 initiator 共享同一个全局 pending 位。
- slot 发布通过 `published_seq: Release` 完成，消费端通过
  `PublishedShootdownRequest` 在 `Acquire` 后读取其余字段。
- `ActiveShootdownSlot` 把“抢占关闭 + slot active”绑定成一个守卫类型，
  防止调用者忘记成对维护这两个约束。

### Fast path gate

- `PENDING_EPOCH_BY_CPU[target]` 是 **每目标 CPU 的单调计数**。
- 每次对某个目标 CPU 发布 shootdown 请求，先 publish request，再 bump epoch，
  再发 IPI。
- 目标 CPU 的 `handle_shootdown()` 先比较 epoch，避免普通 IPI 也 O(CPU_NUM)
  扫描全部 request slots。

这里用单调计数而不是布尔 `pending`，是为了避免多个 initiator 并发发布时的
丢事件窗口。

## 设计决策

### 为什么 IPI 队列和 TLB 协议共用一个 IPI 向量

优点：

- 避免为每类跨核控制流单独申请中断向量；
- 统一 `kirq::notify_cpu(kbuild_config::IPI_IRQ, ...)` 用法；
- 让 IPI handler 成为单一跨核控制入口。

代价：

- handler 必须区分 TLB 协议和普通回调；
- TLB 路径需要 fast path gate，避免普通 IPI 也走完整 slot 扫描。

### 为什么 TLB shootdown 采用“每发起 CPU 一个 slot”

旧的单全局 slot 设计会让不同 initiator 的并发请求互相覆盖或互锁。  
按 initiator 分 slot 后：

- 请求身份变成 `(initiator_cpu, request_seq)`；
- 远程 CPU 对每个 initiator 分别 ack；
- 不同 initiator 可以并发发布，而不会共享同一个 completion 状态。

### 为什么把发布协议封成 `PublishedShootdownRequest`

`is_flush_all` 和 `published_vaddr` 的读取依赖 `published_seq` 的
Release/Acquire 链。直接在调用点散落 `Relaxed` load 容易让维护者误改顺序。

把它收进一个“已发布请求视图”类型后，协议边界变成显式的：

- 先通过 `load_published_request()` 建立 Acquire；
- 再通过视图读取其余字段。

### 为什么 `notify_cpu()` 的发布顺序契约下沉到中断控制器实现

“先发布普通内存中的请求状态，再发送 IPI”是 IPI 原语的契约，不只是
TLB shootdown 的私有需求。  
因此 `IntrManagerIf::notify_cpu` 明确要求各架构后端保证
publish-before-notify 语义，而不是在单个调用点散落 fence。

### 为什么停机协议用标志位而不是 IPI 回调

- 停机必须在“目标 CPU 的 IPI 队列可能已满”的极端场景下仍然可达；
- 停机路径不允许分配内存；
- 停机语义是全局单发的（一次只有一个 orchestrator），用一个状态字加
  per-CPU ack 数组即可，
  无需复用回调队列的排队/确认机制。

### 为什么停机状态用单个原子值表达

请求标志和 orchestrator 身份本质上是同一个状态（"是否有人发起停机、
是谁"）。拆成 `STOP_REQUESTED` + `STOP_ORCHESTRATOR` 两个变量时，无论
先发布哪个都留有窗口：先发布请求再写 id，窗口内一个无关 IPI（如 TLB
shootdown）到达 orchestrator 自身，`handle_stop_request` 会看到"已请求
停机但 id 仍是 `NO_ORCHESTRATOR`"，orchestrator 无法识别自己，随即 ack
并 park——唯一应走到平台终点的 CPU 自停，系统既不 halt 也不断电；而仅
交换两条语句的顺序时，并发后到者的 store 又会覆盖先到者的 id。因此合并
为单个 `AtomicUsize`（`usize::MAX` 表示无请求，CPU id 表示请求已由该
CPU 发布），一次 `compare_exchange` 同时完成选主与发布，观察者 Acquire
读到请求时必然同步看到正确的 orchestrator id，协议中不再存在需要额外
排序保证的中间状态。

### 为什么停靠前要停靠本地 NMI 源

`karch::stop_cpu()` 现在是 `-> !` 的终结停靠：AArch64 侧以 `msr daifset,
#0xf` 全掩 DAIF 四类异常（debug、SError、IRQ 含 pseudo-NMI、FIQ），其它
架构屏蔽本地可屏蔽中断并循环进入等待指令。但 CPU 级屏蔽挡不住真正的
NMI（x86 的 NMI 不受 IF 控制）。若不先把 NMI 源停掉：

- 带 NMI 看门狗的平台（如 x86 NMI watchdog）上，停靠中的 CPU 会被周期性
  NMI 反复唤醒，无法真正停下；
- NMI 驱动的 hard-lockup 看门狗会把“故意停机”误判为 lockup，panic 后走
  平台断电，使 `halt` 与 `power_off` 的区分失效。

因此在 park 前调用 `khal::quiesce_nmi()`；它在无 NMI 设施的平台是 no-op，
不引入新的架构耦合。在 pseudo-NMI 已被 DAIF 全掩挡住的 AArch64 上，停靠
PMU 源还顺带让看门狗计数器彻底安静，属于纵深防御。orchestrator（终端
CPU）自身的本地 NMI 则由 `khal::power::halt()` / `power_off()` 在调用裸
平台终点前停靠，因此无论是否注册了 SMP stop 钩子（如 UP 构建），终端 CPU
都不会被看门狗 NMI 唤醒。

### 为什么 orchestrator 全程关闭抢占

认领的 orchestrator id、`AllButSelf` 广播和等待循环都以发起时的
`current_cpu_index` 为锚。调用来自普通系统调用上下文（如 `sys_reboot`）时
中断开启、任务可抢占：若协议中途被迁移，被离开的 CPU 会被所有 CPU 当作
orchestrator 而永不 park，而真正的 orchestrator 却在等它 ack，直到超时。
`NoPreempt` 守卫把发起 CPU 钉住，代价只是一次每 CPU 计数。

### 为什么等待集只含 IPI 队列已就绪的 CPU

等待集在发布停机请求时一次性快照，只包含 IPI 队列已初始化的 present CPU。
尚未跑到 `kipi::init()` 的 present CPU（AP 带起较慢或启动失败）收不了
共享 IPI，也永远无法 ack；把它留在等待集里意味着每次停机都白等满 1 秒
超时并打出误导性告警。广播仍然发往除自身外的全部 CPU，晚就绪的 CPU 在
后续任意 IPI 进入 `handle_stop_request` 时照样 park，因此缩小等待集只影响
等待，不影响停机覆盖面，与 Linux `smp_send_stop()` 的 best-effort 语义
一致。

## Drop / 资源释放

- 通用回调的生命周期由 `Callback` / `MulticastCallback` 自然管理；
- 广播回调通过 `Arc` 克隆，再在目标 CPU 上转换成一次性 `Callback`；
- `IpiEventQueue` 中的事件在 `pop_one()` 后被消费；
- `ActiveShootdownSlot` 通过 `Drop` 自动清除 active 状态，防止异常路径遗留 slot 占用；
- `kipi` 本身不拥有跨 CPU 长生命周期的内存池或设备资源，主要资源都是
  per-CPU 队列和原子状态。

# kipi — 设计文档

## 定位

`kipi` 是 X-Kernel 的跨 CPU 协调模块，向上提供：

- 通用 IPI 回调投递与广播；
- I-cache 跨核失效的实现绑定；
- TLB shootdown 的跨核通知与确认协议。

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
│   └── tlb.rs
└── Cargo.toml
```

其中：

- `lib.rs`：对外 API、每 CPU 事件队列、IPI handler；
- `event.rs`：单播/广播回调模型；
- `queue.rs`：每 CPU FIFO 队列；
- `icache.rs`：I-cache 跨核 flush 的 crate interface 实现；
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

## Drop / 资源释放

- 通用回调的生命周期由 `Callback` / `MulticastCallback` 自然管理；
- 广播回调通过 `Arc` 克隆，再在目标 CPU 上转换成一次性 `Callback`；
- `IpiEventQueue` 中的事件在 `pop_one()` 后被消费；
- `ActiveShootdownSlot` 通过 `Drop` 自动清除 active 状态，防止异常路径遗留 slot 占用；
- `kipi` 本身不拥有跨 CPU 长生命周期的内存池或设备资源，主要资源都是
  per-CPU 队列和原子状态。

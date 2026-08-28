# kdevice — 安全与可靠性分析

## 信任模型

```text
    kdriver (bus enumeration,    kclass / knet / ...
    probe, adoption, remove)         │
         │                           │ subscribe_event
         │ lifecycle APIs            │ query / snapshot
         ▼                           ▼
┌─────────────────────────────────────────────┐
│ kdevice                                     │
│                                             │
│ 全部安全 Rust — 零 unsafe 块                 │
│                                             │
│ 安全保证来自：                               │
│  ├─ SpinNoPreempt 锁保护所有可变共享状态      │
│  ├─ AtomicU8 + CAS 管理设备生命周期状态      │
│  ├─ Arc 引用计数保证对象跨锁存活              │
│  ├─ BTreeMap 防止 ID 碰撞                   │
│  └─ 严格的锁顺序规则预防死锁                  │
└──────────────┬──────────────────────────────┘
               │
               │ DeviceEvent 分发
               ▼
    kclass (event subscriber)
```

- `kdevice` 信任调用者（`kdriver`）正确实现了 `DeviceDriver` trait，
  尤其是 `probe_device` 在成功时完成设备初始化、在失败时不泄露资源。
- `kdevice` 信任调用者遵守锁顺序规则（不在 per-object 锁内回调 registry）。
- `kdevice` 信任 subscriber callback 不回调 driver-core mutator（遵守 subscriber contract）。
- `kdevice` 只从 process/task 上下文被调用（不使用 IRQ-safe 锁）。

## 外部边界 / 攻击面

`kdevice` 是纯内存数据结构层，不直接接触硬件、firmware 或网络输入。
其安全边界来自：

- **并发正确性**：多个 CPU 通过 kdriver 的路径同时访问 DeviceRegistry/BusInstance/DeviceObject。
  锁顺序违规可能导致死锁；CAS 逻辑错误可能导致状态机异常。
- **状态机完整性**：DeviceState 转换的正确性影响整个设备模型。
  非法状态转换（如 Removing→Active）可能导致 use-after-free。
- **subscriber callback 安全**：subscriber 在 kdevice 锁外被调用，
  但若 callback 违反 contract（回调 driver-core mutator），可能导致死锁或状态破坏。
- **ID 分配器溢出**：`AtomicU64` 的 ID 计数器理论上可能溢出（约 1.8×10^19 次分配），
  超过后会 panic。
- **Arc 引用循环**：DeviceObject 的 parent→child 和 child→parent 双向 `Arc` 引用
  在 remove 路径中通过显式 detach 打破，若遗漏会导致内存泄漏。

威胁分析重点应覆盖：

- 锁顺序违规是否可能发生（audit 所有 registry guard 持有期间的回调路径）；
- DeviceState CAS 循环在并发场景下的正确性；
- `begin_removing` 与 `try_acquire` 之间的竞态是否可能产生 use-after-free；
- subscriber callback panic 是否影响其他 subscriber 或驱动核心；
- `DeviceRegistry::find_bus_type` 的 panic-on-missing 是否可能在异常路径触发。

## unsafe 代码清单

**kdevice 不包含任何 unsafe 代码。**

整个 crate（约 3500 行）使用 100% 安全 Rust 实现。
所有并发控制通过 `SpinNoPreempt`、`AtomicU8`、`AtomicU64`、`AtomicUsize` 和 `Arc` 的标准安全抽象完成。

## 内存安全不变量

1. **锁顺序规则**：Registry → (drop guard) → per-object。所有 caller 必须遵守。
   `DeviceRegistry` 的 `find_bus_type` 方法注释了 panic-on-missing 行为，
   要求调用者在 init 阶段先注册 bus type。
2. **DeviceObject::begin_removing 原子性**：CAS 循环是唯一的 `→Removing` 提交点。
   CAS 成功后状态不可逆，且必须检查 usage 计数为 0。
   违反此规则可能导致 use-after-free。
3. **DeviceUse RAII guard**：`try_acquire` 使用 `fetch_add` + 状态检查；
   `begin_removing` 在 CAS 后检查 usage 计数。两者协同保证：
   要么 `DeviceUse` 存在时 `begin_removing` 被拒绝，
   要么 `begin_removing` 成功后 `try_acquire` 看到非 Active 状态并回滚计数。
4. **DeviceDesc Probing 互斥**：`mark_device_desc_probing` 仅在 `Pending` 状态时 CAS 到 `Probing`。
   防止同一描述符被并发 probe。
5. **Subscriber contract**：subscriber callback 在无 driver-core 锁的情况下运行，
   且不允许回调 mutator。违反此规则可能导致死锁或 re-entrant state 破坏。
6. **Arc 生命周期**：所有跨锁边界的对象通过 `Arc` 共享。
   Registry guard drop 后对象仍存活，防止 use-after-free。
7. **ID 唯一性**：`AtomicU64` 单调递增，从不重用。即使 ID 计数器回绕，
   BTreeMap 中残留的旧条目也不会与新 ID 冲突（概率极低但理论上可能）。
8. **devres LIFO 顺序**：`run_cleanups` 按 `Vec::pop()`（LIFO）执行清理回调，
   保证最后申请的资源最先释放。
9. **remove 不可逆**：`begin_removing` CAS 成功后，即使 `driver.remove()` 或 `bus_type.remove()` 失败，
   也不会回滚到 Active 状态。这避免将部分清理的设备暴露为"可用"。
10. **parent/child detach**：remove 路径中从 parent 的 children 列表中移除 child，
   并清空 child 的 parent 指针。防止 stale parent 引用。

## 线程安全

| 类型 | Send 条件 | Sync 条件 |
|------|-----------|-----------|
| `DeviceRegistry` | 字段满足 Send | `SpinNoPreempt` 提供内部可变性 |
| `DeviceObject` | `Arc<DeviceObject>` 满足 Send + Sync | `AtomicU8` + `SpinNoPreempt` + `AtomicUsize` |
| `BusInstance` | `Arc<BusInstance>` 满足 Send + Sync | 内部 `SpinNoPreempt` 保护所有可变字段 |
| `BusTypeObject` | `Arc<BusTypeObject>` 满足 Send + Sync | 内部 `SpinNoPreempt` 保护 |
| `DriverObject` | `Arc<DriverObject>` 满足 Send + Sync | 内部 `SpinNoPreempt` + `AtomicU64` |
| `DeviceTopology` | `Vec` 纯数据，满足 Send | 不可变快照，满足 Sync |
| `DeviceEventSubscribers` | `Vec<Arc<dyn Fn>>` 满足 Send | 通过 `DeviceRegistry` 的 `SpinNoPreempt` 串行访问 |

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | 锁顺序违规导致死锁 | 高 | 在 per-object 锁内回调 `device_registry()` | 锁顺序规则文档化；`BusInstance`/`DeviceObject` 方法标注 `pub(crate)` 限制直接使用 |
| T-02 | `begin_removing` 与 `try_acquire` 竞态导致 use-after-free | 高 | 时序：try_acquire fetch_add 后、state check 前，begin_removing CAS 成功 | `try_acquire` 先 `fetch_add` 再检查 state；不 Active 则 `fetch_sub` 回滚。`begin_removing` CAS 后检查 usage。两者保证互斥 |
| T-03 | 同一描述符并发 probe | 中 | 两个路径同时 probe 同一 `DeviceDescId` | `mark_device_desc_probing` 仅在 Pending→Probing 时成功，并发者收到 Probing 状态并返回 Requeue |
| T-04 | subscriber callback panic 阻断其他 subscriber | 中 | 某个 callback panic，后续 callback 未被调用 | 当前无 `catch_unwind`；callbacks 按迭代顺序调用；依赖 subscriber 实现质量 |
| T-05 | subscriber callback 中回调 mutator 导致死锁 | 高 | subscriber 中调用 `probe_device_desc` 等 | subscriber contract 明确禁止；审查 subscriber 实现是调用者责任 |
| T-06 | ID 计数器溢出 panic | 低 | `AtomicU64` 达到 `u64::MAX` | 64-bit 宽度在合理使用下不可能溢出（每秒 10^6 次分配 = 5.8×10^5 年） |
| T-07 | `find_bus_type` panic on missing bus type | 中 | 在 `register_bus_type` 之前创建 bus instance | `default_bus_manager` 规范顺序：先 register_bus_type 再 register_bus_instance |
| T-08 | 驱动 probe 失败后 devres 未清理 | 中 | 驱动在 probe 中注册 devres 后 panic（unwinding 不执行 cleanup） | x-kernel 为 `#![no_std]`，当前无 unwinding；probe 失败通过 `run_cleanups` 显式清理 |
| T-09 | 设备 remove 后 parent/child 关系残留 | 中 | remove 路径中忘记 detach parent | `remove_device_from_index` 从 parent children 列表中移除 child，并清空所有 children 的 parent 指针 |
| T-10 | `DeviceRecord` 快照与 live 对象不一致 | 低 | 快照在锁内构建，但某些字段在锁外被读取 | `record_snapshot()` 先读 `AtomicU8` lifecycle，再持 per-object lock 读 state 字段 |
| T-11 | Arc 引用循环导致设备/总线对象内存泄漏 | 中 | parent↔child 双向 Arc 引用未打破 | remove 路径中显式 `detach_child` + `set_parent(None)`；controller↔bus 的 `set_child_bus`/`set_controller` 由 backend 管理 |

影响等级定义：

- 高：导致 UB、内存破坏、死锁。
- 中：导致 panic、资源泄漏、数据不一致。
- 低：导致性能退化、日志丢失、功能降级。

## 故障模式与影响分析

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | 驱动注册前 bus type 未注册 | `register_bus_type` 在 `register_driver_object`/`register_bus_instance` 之后调用 | `find_bus_type` panic | 内核启动中断 | 1 | `default_bus_manager` 先注册 bus type；init 顺序写死在代码中 |
| F-02 | probe 路径因资源不足创建 DeviceObject 失败 | `Arc::new` 或 `Vec` 内存分配失败（OOM） | 当前 probe 失败，描述符 requeue | 设备可能在后续 reprobe 中激活 | 3 | 当前不处理 OOM（`#![no_std]` 环境）；后续可实现 OOM 回调 |
| F-03 | `begin_removing` CAS 循环被并发 writer 饿死 | 多个线程持续修改 lifecycle，CAS 一直失败 | 该 remove 请求延迟 | 设备 remove 延迟 | 4 | CAS 使用 `compare_exchange_weak` + retry loop；竞争窗口极小 |
| F-04 | remove 路径中 `driver.remove()` panic | 驱动 remove 实现有 bug | remove 流程中断 | 设备状态卡在 Removing，无法彻底清理 | 2 | 当前无 `catch_unwind`；依赖驱动 remove 实现质量 |
| F-05 | subscriber callback 中分配内存失败 | callback 中 `Arc::new` OOM | 取决于 callback 实现 | 可能导致 panic 或静默丢失事件 | 3 | kdevice 不在 callback 中分配内存；OOM 由 subscriber 自行处理 |
| F-06 | `device_records_snapshot` 在大量设备时内存压力大 | 数千个活跃设备需要分配大 Vec | 快照耗时增加，可能 OOM | 快照调用者受影响（如 /proc 读取） | 4 | BTreeMap 迭代器按 ID 顺序生成记录；调用者可考虑分页 |
| F-07 | 同一 DeviceId 被两次 `add_device` | 两次 adoption 使用相同 ID | 第二次 add_device 替换第一次的条目 | 旧 DeviceObject 可能被 drop，外部 Arc 持有者仍可访问 | 4 | ID allocator 单调递增保证不重复；adoption 每次分配新 ID |
| F-08 | `desc.parent` 指向已移除的 parent | parent device 在 child 发布前被 remove | `attach_device_parent` 在 publish 后调用，parent 可能已移除 | child 的 parent 为 None，设备树不完整 | 4 | `attach_device_parent` 失败仅记录 warn，不阻断 child 激活 |
| F-09 | BusInstance.devices 与 DeviceRegistry.devices 不一致 | 某个路径只更新了一处 | 快照可能漏掉设备或包含已移除设备 | 查询结果不一致 | 3 | add_device 在 registry 锁内同时更新两处；remove 同理 |
| F-10 | 描述符 `attempted` 列表无限增长 | 大量驱动对同一描述符 probe 失败 | attempted Vec 内存增长 | 单描述符内存占用增加 | 4 | device remove 后 requeue 清空 attempted；单描述符最多 attempted 驱动数量有限 |

严重度定义：

- 1：致命，系统崩溃、数据丢失。
- 2：严重，功能不可用，需重启恢复。
- 3：一般，功能降级，可自动恢复。
- 4：轻微，影响有限，用户可容忍。

## 故障管理

- `DeviceRegistry::find_bus_type` 在 bus type 未注册时 panic——这是初始化顺序错误，不应在运行时发生。
- `DeviceObject` 的 `state()` 在 `from_u8` 解析失败时返回 `DeviceState::Removed`（而非 panic），防止内存损坏影响热路径。
- 所有生命周期 API 返回 `Result<_, DriverError>`（`InvalidInput` / `BadState` / `ResourceBusy` / `Unsupported` 等）。
- `remove_device_managed` 中的 `driver.remove()` 和 `bus_type.remove()` 错误仅记录日志，不阻断移除流程（"remove never fails" 语义）。
- `probe_device_desc` 的 probe 失败执行完整回滚（`run_cleanups` + `detach_driver` + requeue），不会泄露部分初始化的设备。
- subscriber callback 的 panic 当前无保护，但 x-kernel 在 `#![no_std]` 环境中不使用 unwinding（panic = abort），因此不会跳过后续 subscriber。

## 隐私分析

`kdevice` 处理设备身份信息（`DeviceIdentity`：PCI vendor/device ID/class、platform alias/firmware_id）、
总线拓扑信息（`DeviceLocation`、`BusInfo`）和设备元数据（`DeviceRecord`）。
这些数据在 `Debug` 输出和快照函数中可见。

模块不处理用户数据，不持久化，不记录日志（所有日志由上层 `kdriver` 产生）。

## 已知限制

- 无 `catch_unwind` 保护：subscriber callback 和 `driver.remove()` 的 panic 可能导致状态不一致（但在当前 `panic=abort` 环境下无 unwinding）。
- ID allocator 使用 64-bit 计数器，理论上会溢出（实际不可能，但未做编译期或运行时检查）。
- `DeviceRegistry` 使用 `BTreeMap` 而非 `HashMap`：插入/查找为 O(log n) 而非 O(1)，但免去了哈希函数的依赖。
- subscriber 不支持取消订阅（unsubscribe）：一旦注册，callback 在 registry 生命周期内永久有效。
- `DeviceTopology` 每次调用 `snapshot()` 都分配新的 Vec，不缓存。
- parent/child 关系只有一级（不跨 bus 层级），多级设备树需要扩展 `children` 为递归结构。
- 无 IOMMU 或设备隔离相关的类型抽象；这类能力由具体内核适配层处理。

## 审计清单

修改本模块时需验证：

- 无新增 `unsafe` 块（当前为零 unsafe，保持该状态）。
- 新增 per-object lock 时确认不违反锁顺序规则（Registry → per-object）。
- 新增 lifecycle 状态转换时更新 `DeviceState` 的 `as_u8`/`from_u8` 和 `repr(u8)` 映射。
- 新增 subscriber event kind 时更新 `DeviceEventKind::COUNT` 和 `index()` 映射。
- 新增 registry 查询时不持有 per-object lock。
- `DeviceObject::begin_removing` 的 CAS 逻辑修改需经并发安全审查。
- 新增 `DeviceDriver` 方法时确认 `DriverObject` 的委托路径完整。
- 新增 `DeviceMatcher` 实现时确认 `matches()` 是纯函数（无副作用，无锁获取）。

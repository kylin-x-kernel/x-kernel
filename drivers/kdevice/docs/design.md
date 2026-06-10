# kdevice — 设计文档

## 定位

`kdevice` 是 x-kernel 的共享设备模型类型 crate。
它为总线(bus)、设备(device)、驱动(driver)三元组提供稳定的类型定义、
全局对象注册表、生命周期状态机和事件分发基础设施。

`kdevice` 是设备驱动栈的最底层——`kdriver`（驱动编排）和 `kclass`（类型化 class 层）
都构建在它之上，但 `kdevice` 自身不依赖它们。
所有持久化的设备拓扑信息（总线实例、设备对象、驱动对象、发现描述符）都在 `kdevice` 中管理。

目标读者是实现新总线类型匹配器、修改设备生命周期状态机或扩展全局注册表查询能力的开发者。

## 背景

Linux 内核的设备模型通过 `device`、`device_driver`、`bus_type` 三个核心结构体
以及 `devres`（device resource）机制提供设备发现→绑定→激活→移除的完整生命周期管理。
x-kernel 的 `kdevice` 将这套机制落入 Rust 的类型系统和所有权模型：

- **类型安全**：总线实例、设备对象、驱动对象各自是独立的 Rust 类型，
  通过 `Arc` 共享，通过 `SpinNoPreempt` 保护可变状态；
- **所有权明确**：`DeviceObject` 拥有其 devres 清理回调；
  `DeviceRegistry` 拥有全局索引；
  生命周期状态转换由 `AtomicU8` + CAS 保证原子性；
- **事件驱动**：生命周期事件（Published/Matched/Bound/Activated/Removed）
  通过分桶(bucketed) subscriber 机制分发给 `kclass` 等观察者。

## 范围

涉及的源文件：

```text
drivers/kdevice/
├── Cargo.toml
├── docs/
│   ├── design.md
│   └── security.md
└── src/
    ├── lib.rs                    # crate 入口，re-export 所有公开类型
    ├── bus/
    │   ├── mod.rs                # BusId, BusInfo, BusInstance
    │   └── bus_type.rs           # BusTypeId, BusType trait, BusTypeObject,
    │                             #   PciBusTypeMatcher, PlatformBusTypeMatcher
    ├── device/
    │   ├── mod.rs                # 设备子模块 root
    │   ├── desc.rs               # DeviceDesc, DeviceId, DeviceLocation,
    │   │                         #   DeviceIdentity, DeviceState, DeviceRecord
    │   ├── object.rs             # DeviceObject（核心运行时对象）,
    │   │                         #   DeviceUse（RAII 使用计数 guard）
    │   ├── handles.rs            # BusHandle, DeviceCore, DriverCore
    │   └── resource.rs           # 资源类型 re-export + IRQ trigger 转换
    ├── driver/
    │   └── mod.rs                # DriverId, DriverInfo, DriverObject,
    │                             #   DeviceDriver trait, 各种 DeviceMatcher,
    │                             #   ProbeStats, priority 模块
    ├── lifecycle/
    │   ├── mod.rs                # 生命周期编排：
    │   │                         #   register_bus_instance, register_driver_object,
    │   │                         #   probe_device_desc, adopt_active_device,
    │   │                         #   remove_device_managed, attach/detach parent
    │   ├── dispatch.rs           # 事件分发与状态转换通知
    │   ├── event.rs              # DeviceEvent, DeviceEventKind
    │   └── subscribers.rs        # DeviceEventSubscribers（分桶存储）
    ├── registry/
    │   └── mod.rs                # DeviceRegistry（全局 BTreeMap 索引）,
    │                             #   查询/快照/测试辅助函数
    └── topology/
        └── mod.rs                # DeviceTopology（只读拓扑快照）,
                                  #   BusView, DeviceCoreView, DriverCoreView
```

## 架构

```text
    kdriver (enumerate, probe)    kclass (event subscriber)
         │                              │
         │ register_bus_instance        │ subscribe_device_event_kind
         │ register_driver_object       │
         │ device_desc_add              │
         │ probe_device_desc            │
         │ adopt_active_device          │
         │ remove_device_managed        │
         ▼                              ▼
    ┌─────────────────────────────────────────────┐
    │ kdevice                                      │
    │                                              │
    │  ┌──────────────────────────────────────┐    │
    │  │   lifecycle::mod                      │    │
    │  │   ├─ probe pipeline (desc → publish)  │    │
    │  │   ├─ adoption (early device handoff)  │    │
    │  │   ├─ remove (managed teardown)        │    │
    │  │   └─ parent/child attach/detach       │    │
    │  └──────────────┬───────────────────────┘    │
    │                 │                             │
    │  ┌──────────────┴───────────────────────┐    │
    │  │   lifecycle::dispatch                 │    │
    │  │   mark_matched → bind_to_driver →     │    │
    │  │   activate → dispatch_event           │    │
    │  └──────────────┬───────────────────────┘    │
    │                 │                             │
    │  ┌──────────────┴───────────────────────┐    │
    │  │   lifecycle::subscribers              │    │
    │  │   分桶存储: [Published][Matched]       │    │
    │  │            [Bound][Activated][Removed] │    │
    │  └──────────────────────────────────────┘    │
    │                                              │
    │  ┌──────────────────────────────────────┐    │
    │  │   registry::DeviceRegistry            │    │
    │  │   (SpinNoPreempt 全局锁)               │    │
    │  │                                       │    │
    │  │   descriptors: BTreeMap<DescId, ...>  │    │
    │  │   devices:     BTreeMap<DeviceId, ...>│    │
    │  │   buses:       BTreeMap<BusId, ...>   │    │
    │  │   bus_types:   Vec<BusTypeObject>     │    │
    │  │   drivers:     BTreeMap<DriverId, ...>│    │
    │  │   subscribers: DeviceEventSubscribers │    │
    │  └──────────────────────────────────────┘    │
    │                                              │
    │  ┌──────────┐ ┌───────────┐ ┌────────────┐  │
    │  │BusInstance│ │DeviceObject│ │DriverObject│  │
    │  │(per-bus  │ │(per-device│ │(per-driver │  │
    │  │SpinLock) │ │SpinLock + │ │SpinLock +  │  │
    │  │          │ │AtomicU8)  │ │AtomicU64)  │  │
    │  └──────────┘ └───────────┘ └────────────┘  │
    │                                              │
    │  ┌──────────────────────────────────────┐    │
    │  │   topology::DeviceTopology            │    │
    │  │   只读快照，支持按 bus/driver 过滤    │    │
    │  └──────────────────────────────────────┘    │
    └─────────────────────────────────────────────┘
```

| 组件 | 职责 |
|------|------|
| `DeviceRegistry` | 全局对象索引；BTreeMap 管理 descriptors/devices/buses/drivers；ID 分配 |
| `DeviceObject` | 核心运行时设备对象；`AtomicU8` 生命周期状态 + CAS 转换；devres 清理链表；usage 计数 |
| `DeviceUse` | RAII guard；持有期间阻止设备 remove |
| `BusInstance` | 运行时总线实例；controller / devices / drivers 列表；probe 统计 |
| `BusTypeObject` | 总线匹配域；管理 pending descriptor 队列和已注册 driver；委托 BusType trait 做匹配 |
| `DriverObject` | 已注册驱动对象；bound device 列表；probe 统计 |
| `DeviceDesc` | 发现阶段描述符；携带 bus_id/location/identity/transport/resources |
| `DeviceDriver` trait | 驱动接口：name/device_kind/bus_types/matcher/probe_device/remove/suspend/resume/shutdown |
| `DeviceMatcher` trait | 开放式匹配器；内置 PciIdsMatcher/VirtioTypeMatcher/CompatibleAliasMatcher/FirmwareMatchSpec/NeverMatcher |
| `DeviceEvent` / `DeviceEventKind` | 5 种生命周期事件；分桶 subscriber |
| `DeviceTopology` | 只读拓扑快照；提供按 bus/driver 过滤的迭代器 |
| 生命周期 API | `register_bus_instance`、`register_driver_object`、`probe_device_desc`、`adopt_active_device`、`remove_device_managed` |

## 状态机

### DeviceRecord 生命周期

```text
                         ┌────────────┐
                         │ Discovered │  ← device_desc_add / adopt_active_device
                         └─────┬──────┘
                               │ probe_device_desc 找到匹配驱动
                               ▼
                         ┌────────────┐
                         │  Matched   │  ← mark_device_matched()
                         └─────┬──────┘
                               │ bind_device_to_driver()
                               ▼
                         ┌────────────┐
                         │   Bound    │  ← driver_id/name/kind 已记录
                         └─────┬──────┘
                               │ probe_device() 返回 Ok
                               ▼
                         ┌────────────┐
                         │   Active   │  ← activate_device();
                         │            │     触发 Activated 事件
                         └─────┬──────┘
                               │ remove_device_managed()
                               ▼
                         ┌────────────┐
                         │  Removing  │  ← begin_removing() CAS 提交点
                         │            │     driver.remove() + bus_type.remove()
                         │            │     + run_cleanups()
                         └─────┬──────┘
                               │ remove_device_from_index()
                               ▼
                         ┌────────────┐
                         │  Removed   │  ← 触发 Removed 事件
                         └────────────┘
```

| 从 | 到 | 触发条件 |
|----|----|----------|
| — | Discovered | `device_desc_add` 或 `adopt_active_device` 创建 DeviceObject |
| Discovered | Matched | `probe_device_desc` 找到最佳匹配驱动 |
| Matched | Bound | `bind_device_to_driver` 记录 driver_id/name/kind |
| Bound | Active | `DeviceDriver::probe_device` 返回 `Ok(())` |
| Bound | Discovered (requeue) | probe 失败，desc requeue，device 对象丢弃 |
| Active/MBound/Bound | Removing | `remove_device_managed` CAS 成功 |
| Removing | Removed | driver.remove + bus_type.remove + run_cleanups 完成后 |

`begin_removing` 使用 `compare_exchange_weak` CAS 循环：
- 只有 `Active`/`Bound`/`Matched`/`Discovered` 状态可转入 `Removing`
- 已有 `Removing` 或 `Removed` → 拒绝（防重入）
- usage 计数非零 → 回滚并返回 `ResourceBusy`
- CAS 成功后状态不可逆

### DeviceDesc 生命周期（描述符优先路径）

```text
                         ┌──────────┐
                         │ Pending  │  ← device_desc_add
                         └────┬─────┘
                              │ mark_device_desc_probing
                              ▼
                         ┌──────────┐
                         │ Probing  │  ← probe_device_desc 持有中
                         └────┬─────┘
                    ┌─────────┼─────────┐
                    │ probe   │ probe   │
                    │ success │ failure │
                    ▼         ▼         │
              ┌──────────┐ ┌──────────┐ │
              │Bound(id) │ │ Pending  │◄┘ requeue
              └──────────┘ └──────────┘
                    │
                    │ remove_device_from_index
                    ▼
              ┌──────────┐
              │ Pending  │  ← 设备已移除，描述符重新开放匹配
              └──────────┘
```

关键语义：
- `Probing` 状态防止并发 probe 同一描述符；
- `attempted` 列表记录已失败驱动，reprobe 时跳过避免无限重试；
- 设备 remove 时关联的描述符回到 `Pending` 且清空 `attempted`，为新驱动提供机会。

## 算法流程

### probe 流水线

`probe_device_desc(id)` → `probe_device_desc_with_drivers(desc, candidates)`：

1. 检查 `desc_probe_outcome`：如果描述符已 `Bound` 且设备处于终态（Active/Removing/Removed），返回 Skipped。
2. CAS 标记描述符为 `Probing`（防并发）。
3. 从 `bus_type` 获取候选驱动列表。
4. 按 `match_desc` 匹配，选最高优先级驱动（排除 `attempted` 中已失败的）。
5. 无可匹配驱动 → requeue 返回 `Requeue`。
6. 从描述符创建 `DeviceObject`（分配 DeviceId，构造 DeviceObject）。
7. 走标准事件序列：`mark_matched` → `bind_to_driver`。
8. 调用 `driver.ops().probe_device(device)`：
   - 成功 → `publish_desc_device`：写入全局索引，添加到 BusInstance，parent attach，触发 Published + Activated 事件。
   - 失败 → `device.run_cleanups()`（devres LIFO）+ `detach_driver()` + 记录 failed driver 到 `attempted` + requeue。

### adoption 路径

`adopt_active_device(adoption)`：用于 boot console、PCI host bridge 等已在早期初始化的设备。

1. 校验 target bus 存在，driver 的 `bus_types` 包含 target bus 的 `bus_type`。
2. 分配 `desc_id` 和 `device_id`。
3. 构造 `DeviceDesc` + `DeviceObject`（跳过 match/probe）。
4. 与 probe 路径走相同的标准事件序列：Matched → Bound → Published → Activated。
5. parent attach（如果 adoption 指定了 parent）。

### remove 路径

`remove_device_managed(id)`：

1. 从 registry 查找 device、driver、bus_type。
2. `device.begin_removing()` — **单次提交点**：
   - CAS 转入 `Removing`，拒绝 `Removing`/`Removed` 状态，拒绝 usage 非零。
3. 调用 `driver.ops().remove(device)` — best-effort，错误仅记录日志不阻止继续移除。
4. 调用 `bus_type.remove(device)` — best-effort。
5. `device.run_cleanups()` — LIFO 执行 devres 清理。
6. `remove_device_from_index(id)` — 从 registry 移除、更新 BusInstance/DriverObject、parent/child 解绑、触发 Removed 事件。

### 驱动注册后的描述符重扫描

`register_driver_object` 注册驱动后立即扫描该驱动 bus_types 中所有 pending 描述符：

1. 从各 `BusTypeObject` 收集 pending descriptor ID。
2. 对每个 descriptor 调用 `probe_device_desc_with_drivers`（candidates 仅含新注册驱动）。
3. 这避免了驱动加载顺序问题：后注册的驱动仍能找到已发现的设备。

## 锁顺序规则

驱动核心使用多个 `SpinNoPreempt` 保护的对象。为防止死锁，所有路径必须遵守：

```
1. Registry (DeviceRegistry)     ← 总先获取
2. BusInstance / BusTypeObject   ← 从 registry 获取 Arc 后，drop registry guard 再访问
3. DeviceObject / DriverObject   ← 最内层
```

**规则**：
- 获取 registry guard → 快照需要的 `Arc` handle → **drop registry guard** → 访问 per-object lock。
- **禁止**：在 per-object lock 内回调 `device_registry()`。
- **禁止**：在 lifecycle subscriber callback 中调用 driver-core mutator。

`DeviceObject::begin_removing` 的 CAS 循环不需要 per-object spinlock —
lifecycle 字段是 `AtomicU8`，允许 lock-free read + CAS write。

## 并发模型

- **`DeviceRegistry`**：全局 `SpinNoPreempt`；所有 lookup/add/remove 在此锁内。
- **`BusInstance`**：内部 device/driver/controller 列表各用 `SpinNoPreempt` 保护。
- **`DeviceObject`**：`lifecycle` 用 `AtomicU8` + CAS（lock-free read 路径）；`state`（parent/children/driver 绑定）用 `SpinNoPreempt`；`usage` 用 `AtomicUsize`。
- **`DriverObject`**：`bound_devices` 用 `SpinNoPreempt`；`probe` 统计用 `AtomicU64`。
- **`BusTypeObject`**：buses/pending_descriptors/drivers 列表各用 `SpinNoPreempt`。
- **`ProbeCounters`**：`AtomicU64` + `Relaxed` ordering（仅统计用途）。
- **ID 分配器**：`AtomicU64` + `Relaxed`，在 registry 锁外进行。

所有锁均为 `SpinNoPreempt`（关抢占不关中断），因此**不能从中断上下文调用**。

## 设计决策

### 描述符优先设计

**选择**：设备发现产生 `DeviceDesc` 描述符，不直接创建 `DeviceObject`。两者生命周期解耦。

**Trade-off**：增加了一层抽象和状态管理，但换取以下好处：
- 描述符可多次匹配（设备 remove 后回到 Pending，允许新驱动接管）；
- 描述符的 `attempted` 列表可追踪已失败驱动，避免重复 probe；
- `DeviceObject` 只有在 probe 时才创建，未匹配设备不浪费运行时对象的内存。

**拒绝的方案**：发现即创建 `DeviceObject`。简化了代码但失去了描述符的独立生命周期管理能力。

### 开放式 DeviceMatcher trait 替代封闭枚举

**选择**：`DeviceMatcher` 是 trait，内置实现（PciIdsMatcher、VirtioTypeMatcher 等）与外部实现平等。

**Trade-off**：动态分发（`&dyn DeviceMatcher`）的微小开销，但换取以下好处：
- 外部 crate 可以定义自定义匹配器；
- 不需要在 `kdevice` 中枚举所有可能的匹配器类型；
- 匹配器可以携带任意状态（如 FirmwareMatchSpec 既实现 `DeviceMatcher`，也实现 `firmware_spec()` 扩展方法）。

**拒绝的方案**：`MatchTable` 枚举。每个新匹配器都要修改 `kdevice`，且无法支持外部定义的匹配逻辑。

### AtomicU8 生命周期 + CAS 转换

**选择**：`DeviceState` 用 `#[repr(u8)]` 编码，`DeviceObject::lifecycle` 用 `AtomicU8` 存储。

**Trade-off**：CAS 循环比简单的 spinlock 更复杂，但换取以下好处：
- 热路径 `state()` 读取无需获取 per-object lock；
- `begin_removing` 是 lock-free 的 CAS 提交点，避免在持有 spinlock 时做复杂的 teardown 决策；
- `try_acquire` 使用 `fetch_add` + 检查状态，与 `begin_removing` 形成正确的并发协议。

**拒绝的方案**：所有生命周期操作在 `SpinNoPreempt` 内进行。简化了并发模型，但 `state()` 成为热路径瓶颈（ISR/poll 路径频繁读取）。

### 事件 subscriber 分桶而非全局广播

**选择**：subscriber 按 `DeviceEventKind` 分 5 个桶存储。

**Trade-off**：5 个 `Vec` 的内存开销略大于单个 `Vec`，但换取以下好处：
- 分发 `Activated` 事件时不扫描 `Removed` 的 subscriber；
- 每个事件的 dispatch 开销与具体 kind 的 subscriber 数量成正比。

**拒绝的方案**：单一 subscriber 列表 + per-event filter。简洁但分发时需要遍历全部 subscriber 并根据 event kind 过滤。

### Probe 失败回滚：devres LIFO + detach_driver

**选择**：probe 失败时：
1. `device.run_cleanups()` — 执行驱动已注册的 devres（LIFO）；
2. `device.detach_driver()` — 清除 driver 绑定信息；
3. 记录 failed driver 到 `attempted` 列表；
4. requeue descriptor。

**Trade-off**：每个 probe 失败都执行完整的回滚，增加失败路径开销，但换取以下好处：
- 部分初始化的设备不会泄露资源（devres 保证清理）；
- attempted 列表保证不会无限重试同一驱动；
- requeue 允许下一个 reprobe 尝试其他驱动。

**拒绝的方案**：probe 失败后直接丢弃 descriptor。该方案无法支持"多个驱动都可能匹配同一设备，按优先级尝试"的场景。

### Registry + Arc snapshot 模式（而非嵌套锁）

**选择**：在 registry 锁内获取 `Arc` handle，drop registry guard 后访问 per-object 数据。

**Trade-off**：每次跨对象访问需要先获取 registry guard 再 drop，增加代码模板，但换取以下好处：
- 消除死锁风险（registry 锁与 per-object 锁从不嵌套）；
- `Arc` 保证对象在 registry guard drop 后仍然存活（引用计数保护）。

**拒绝的方案**：全局大锁或嵌套锁。全局大锁简单但性能差；嵌套锁容易死锁。

## Drop / 资源释放

- `DeviceUse::drop`：递减 `DeviceObject::usage` 计数；计数归零后允许 `begin_removing` 通过。
- `DeviceObject` 不实现 `Drop`——清理由 `remove_device_managed` 显式驱动（devres LIFO）。
- `DeviceRegistry` 不实现 `Drop`——是全局静态对象。
- `DeviceTopology` 是纯数据快照，drop 仅释放 Vec 内存。

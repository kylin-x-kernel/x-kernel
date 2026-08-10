# kclass — 安全与可靠性分析

## 信任模型

```text
    kdriver (probe success)
       │
       │ trusted: parent DeviceObject (bound driver verified),
       │          runtime trait object (driver-constructed)
       v
┌─────────────────────────────┐
│ kclass                      │
│                             │
│ safe boundary               │
│  ├─ ClassRegistry publish/  │
│  │   devices/find/subscribe │
│  ├─ ClassDevice::with()     │
│  │   trait delegation       │
│  └─ Event bridge dispatch   │
│                             │
│ (no unsafe boundary in      │
│  kclass — see below)        │
└──────────────┬──────────────┘
               │
               │ query / subscribe / trait calls
               v
    knet / fs_boot / input subsystem / ...
```

- `kclass` 信任 `kdriver` 在调用 `publish_<class>()` 前已完成驱动 probe，
  `parent` 的 `driver_name()` / `driver_id()` 已设置且驱动匹配已通过 `kdevice` 校验。
- `kclass` 信任 runtime trait object 实现了 `Send + Sync` 且内部并发安全。
- `kclass` 信任 `kdevice` 的 `Activated` / `Removed` 事件在正确的时序分发。
- 上层子系统信任 `kclass` 返回的 `ClassDevice<T>` 在设备 remove 后仍可安全访问（通过 `Arc<DeviceObject>` 持有保证）。

## 外部边界 / 攻击面

`kclass` 是类型化的运行时设备能力注册层，不直接接触硬件或外部输入。
其攻击面主要来自：

- **kdriver publish 输入**：驱动传入的 `parent: Arc<DeviceObject>` 和 `runtime: T`。
  kclass 假设 `parent` 已通过 `kdevice` 的完整 probe 流水线——驱动匹配已校验、
  `parent.state` 状态转换受 `kdevice` 内部锁保护。
- **事件桥接**：`kdevice` 分发的 `Activated` / `Removed` 事件到达时序。
  kclass 信任 kdevice 在状态迁移完成后才分发事件。
- **ClassDevice trait delegation**：上层子系统通过 `ClassDevice<T>` 调用 trait 方法时，
  委托到 runtime trait object。kclass 自身不做参数校验——信任各 subsystem 和驱动完成校验。

威胁分析重点应覆盖：

- 事件桥接的回调是否可能在错误的设备状态下被触发；
- `ClassDevice` 的 `Arc` 生命周期是否可能在设备 remove 后产生悬垂引用；
- 重复 publish 是否被拒绝且 class-specific publish 是否正确回滚。

## unsafe 代码清单

kclass 自身不包含任何 `unsafe` 代码块。历史上 `DisplayDevice::fb()` 曾通过
`display::FrameBuffer::from_raw_parts_mut` 从裸 vaddr 构造 framebuffer 引用，
该路径已随 framebuffer 直接映射抽象的移除而删除：`/dev/fb0` 现由 `fbdevice` 的
fbdev emulation（shadow buffer + scanout resource）实现，framebuffer 裸指针构造的
unsafe 边界随之消失，相关安全责任转移到 `fbdevice`（shadow buffer 由 `GlobalPage`
RAII 管理，生命周期与内核等长）。

## 内存安全不变量

1. **ClassDevice Arc 生命周期**：`ClassDevice` 持有 `Arc<ClassDeviceInner<T>>`，
   `ClassDeviceInner` 持有 `Arc<DeviceObject>`。
   只要外部持有 `ClassDevice` 克隆，`DeviceObject` 就不会被释放，
   其 devres 资源（MMIO 映射、IRQ、DMA buffer）保持有效。
2. **ClassDevice 在 remove 后仍可安全使用**：`ClassRegistry::remove` 仅从注册表移除条目；
   外部已持有的 `ClassDevice` 克隆仍有效，其 `with()` 调用通过 `Arc<DeviceObject>` 访问设备资源。
   设备状态变为 `Removing`/`Removed` 后 trait 方法可能返回错误，但不会产生 UB。
3. **注册表锁内操作不回调**：`publish`、`remove` 在 `SpinNoPreempt` 锁内完成，
   不调用外部 callback。callback 在锁外执行。
4. **事件分发无重入**：`notify_class_available` 在 callback 调用前已释放注册表锁，
   callback 中对同一注册表的访问不会死锁。
5. **publish identity 唯一性**：同一 `DeviceId` 的第二次 publish 返回 `AlreadyExists`；
   replacement 必须由 `Removed` 后的新 publish 表达。
6. **device_kind 校验**：`publish_<class>()` 在构造 `ClassDevice` 前校验 runtime 的
   `device_kind` 与注册表类型匹配，不匹配时返回 `InvalidInput` 而不发布。

## 线程安全

| 类型 | Send 条件 | Sync 条件 |
|------|-----------|-----------|
| `ClassDevice<T>` | `Arc<ClassDeviceInner<T>>` 满足 Send，要求 `T: Send + Sync` | `Arc` 提供共享访问 |
| `ClassDeviceInner<T>` | `Arc<DeviceObject>` + `T: Send` + metadata 满足 Send | `Arc<DeviceObject>` + `T: Sync` |
| `ClassRegistry<T>` | `Vec<ClassDevice<T>>` + `Vec<Callback>` 满足 Send | 通过 `SpinNoPreempt` 提供内部可变性 |
| `ClassAvailabilityCallback<T>` | `Arc<dyn Fn(...) + Send + Sync>` 满足 Send + Sync | `Arc` 提供共享访问 |
| `ACTIVATION_BRIDGE` | `LazyInit<()>` 零大小类型 | `LazyInit` 保证初始化线程安全 |

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-02 | 事件桥接在 `kdevice` 未初始化时被触发 | 高 | `ensure_event_bridge` 在 `kdevice::init_device_registry` 之前调用 | kdriver 调用 `publish_*` 前已执行 `init_device_registry`；`ACTIVATION_BRIDGE` 惰性初始化在首次 publish 时触发 |
| T-03 | 注册表 publish 竞态导致设备丢失或重复 | 中 | 同一设备并发 publish | `SpinNoPreempt` 串行化，同 ID 的第二次 publish 返回 `AlreadyExists` |
| T-04 | subscriber callback 中 panic 导致后续 subscriber 未被通知 | 中 | 某个 callback panic，其余 callback 在 `for` 循环中未执行 | callback 在 `catch_unwind` 之外执行；当前无 unwind 保护，依赖 subscriber 实现质量 |
| T-05 | 设备 remove 后 `ClassDevice` 的 `with()` 访问已释放的 runtime | 中 | runtime trait object 的 `Drop` 在 `ClassDeviceInner` drop 之前执行 | `ClassDeviceInner` 的所有字段（包括 runtime）同时 drop；`Arc` 引用计数保证所有引用释放后才 drop |
| T-06 | publish 时 `device_kind` 校验被绕过 | 中 | 驱动错误使用错误的 publish 函数（如对 net 设备调用 `publish_block`） | 显式 `device_kind != $kind` 校验，不匹配时返回 `InvalidInput` 且不发布 |
| T-07 | `find` / `devices` 返回非 `Active` 设备 | 低 | `is_available()` 过滤逻辑被移除或错误实现 | `devices()` 和 `find()` 均通过 `is_available()` 过滤；remove 后设备被 swap_remove |
| T-08 | subscriber 回调中重入同一注册表导致死锁 | 中 | subscriber 回调中调用 `publish_*` / `subscribe_*` 等注册表操作 | `notify_class_available` 在锁外执行回调，重入不会死锁（但可能产生长调用链） |

影响等级定义：

- 高：导致 UB、内存破坏、权限提升。
- 中：导致 panic、服务不可用、数据不一致。
- 低：导致性能退化、日志丢失、功能降级。

## 故障模式与影响分析

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | publish 时 parent 无绑定驱动 | `DeviceObject` 在 publish 前未完成 bind 流程 | publish 返回 `BadState` | 该设备无法发布到 class 注册表 | 3 | `try_new_with_class_metadata` 检查 `driver_name()` 和 `driver_id()` |
| F-02 | publish 时 device_kind 不匹配 | 驱动将 net 设备传入 `publish_block` | publish 返回 `InvalidInput` | 该设备无法发布 | 4 | `kind` 校验在构造 `ClassDevice` 前完成 |
| F-03 | 事件桥接未注册 | `ensure_event_bridge` 未被调用（无 class feature 启用） | 无 Activated/Removed 事件分发 | 设备状态变更不影响 class 注册表 | 4 | `ensure_event_bridge` 在每个 `*_registry_fn()` 中调用 |
| F-04 | subscriber 回调 panic | 回调实现有 bug 导致 unwinding | 后续 subscriber 未被通知 | 部分子系统可能未收到设备可用通知 | 3 | 回调按 Vec 顺序调用；当前未使用 `catch_unwind`；依赖 subscriber 质量 |
| F-06 | 注册表 `devices()` 返回过大的 Vec | 大量设备同时活跃 | 内存分配可能失败 | 调用者收到空 Vec（当前无 OOM 处理） | 4 | `Vec::collect` 可能失败；上层调用者应处理空结果 |
| F-07 | 重复 publish | 驱动绕过正常 remove/add lifecycle | publish 返回 `AlreadyExists` | 新 runtime 不可见 | 4 | 回滚 class-specific publish，保留原 resident object |
| F-08 | input metadata 缺失 | 非 input class 未实现 `class_metadata` 覆盖 | `physical_location()` / `unique_id()` 返回空字符串 | input 设备身份信息缺失 | 4 | `ClassRuntimeMetadata` 默认实现返回 `empty()`；input class 显式覆盖 |

严重度定义：

- 1：致命，系统崩溃、数据丢失。
- 2：严重，功能不可用，需重启恢复。
- 3：一般，功能降级，可自动恢复。
- 4：轻微，影响有限，用户可容忍。

## 故障管理

- publish 校验失败使用 `DriverError` 返回（`BadState`、`InvalidInput`、`AlreadyExists`），不 panic。
- devices、find、subscribe、remove 是 infallible；publish 显式报告重复 identity。
- subscriber callback 的 panic 当前无 unwind 保护，依赖 subscriber 实现质量。
- `ClassDevice` 的 `driver_name()` / `driver_id()` 使用 `expect`——前提是 publish 时已校验，
  如果触发 expect 说明存在 bug（publish 路径未正确校验）。
- kclass 自身不含 `unsafe` 代码块，因此不会因 class adapter 逻辑产生 UB；
  历史上的 framebuffer 裸指针路径已迁移到 `fbdevice` 的 fbdev emulation。

## 隐私分析

`kclass` 不处理用户数据。设备元数据（name、device_kind、driver_name、irq）在日志中以
debug 级别输出，不包含用户进程数据或设备 payload。
input class 的 `physical_location` 和 `unique_id` 是设备标识信息，
不包含用户输入数据。

模块不持久化任何数据；所有状态保持在内存中的 class 注册表和 `ClassDevice` 对象中。

## 已知限制

- subscriber callback 的 panic 无 `catch_unwind` 保护，可能导致后续 subscriber 未被通知。
- `devices()` 每次调用都分配新的 `Vec`，高频率轮询场景可能有分配压力。
- 注册表不支持按条件筛选（如"只列出支持某特性的设备"），调用者需自行过滤。
- 非 input class 无 `ClassDeviceMetadata` 扩展入口；如需添加 class 特定元数据需修改 trait。
- `ClassDevice` 不支持降级通知（如设备即将被 remove 的 pre-notification）。

## 审计清单

修改本模块时需验证：

- 每个 `unsafe` 块均有 `SAFETY:` 注释。
- 新增 class 时在 `class_registries!` 宏调用点添加，而非手写重复逻辑。
- 新增 class 的 runtime type alias（如 `FooDeviceImpl`）在 lib.rs 中声明。
- 新增 class 的 trait delegation impl 覆盖所有必要的 trait 方法。
- 新增 class 在 `prelude` 模块中 re-export。
- publish 路径对 `parent.driver_name()` / `parent.driver_id()` 的校验保留。
- 注册表锁内操作不调用外部 callback（避免死锁）。
- framebuffer 修改遵守 `Arc<DeviceObject>` 生命周期保证。

# kclass — 设计文档

## 定位

`kclass` 是 x-kernel 的类型化运行时设备类层（typed runtime device class layer）。
它在 `kdevice` 的设备核心之上，为每种设备类别（net / block / char / display / input / vsock / 9p）
提供类型安全的发布、枚举、查找和可用性订阅接口。

驱动在 probe 成功后通过 `publish_<class>()` 把运行时能力发布到对应 class 注册表；
子系统（如 `knet`、`fs_boot`、input 子系统）通过 `*_devices()` / `find_*_device()` / `subscribe_*_available()`
发现和使用运行时设备，无需依赖 probe 顺序。

目标读者是实现设备 class 适配、修改 class 注册表语义或添加新设备类别的开发者。

## 背景

Linux 内核的 class 机制（`/sys/class/`）提供按功能类别组织设备的视图，
与总线拓扑（`/sys/bus/`）正交。`kclass` 在 x-kernel 中承担类似角色：

- **kdevice** 管理设备的总线归属、生命周期状态和驱动绑定关系；
- **kclass** 在此之上提供按功能类别（net/block/char/display/input/vsock/9p）的类型化视图；
- **kdriver** 在驱动 probe 成功后调用 `publish_*()` 把运行时能力注入 kclass 注册表。

这种分层使得上层子系统（如网络栈、文件系统）不关心设备在 PCI 还是 platform 总线上，
也不关心 probe 顺序——它们只需要向 class 注册表查询当前可用的设备或订阅未来的可用性通知。

## 范围

涉及的源文件：

```text
drivers/contracts/kclass/
├── Cargo.toml
├── docs/
│   ├── design.md
│   └── security.md
└── src/
    ├── lib.rs       # 宏驱动的 class 注册表 + 设备 trait 委托 + prelude
    └── generic.rs   # ClassDevice<T>、ClassRegistry<T> 泛型原语
```

## 架构

```text
                kdriver (probe success)
                     │
                     │ publish_net / publish_block / publish_display / ...
                     v
    ┌────────────────────────────────────────┐
    │ kclass                                  │
    │                                         │
    │  ┌─────────────────────────────────┐    │
    │  │     ACTIVATION_BRIDGE           │    │
    │  │  (kdevice event subscriber)     │    │
    │  │  Activated → notify_class_avail │    │
    │  │  Removed   → remove_class_device│    │
    │  └─────────────┬───────────────────┘    │
    │                │                        │
    │  ┌─────────────┴───────────────────┐    │
    │  │  Per-class registries           │    │
    │  │  (macro-generated)              │    │
    │  │                                 │    │
    │  │  NET_DEVICES    (SpinNoPreempt) │    │
    │  │  BLOCK_DEVICES  (SpinNoPreempt) │    │
    │  │  CHAR_DEVICES   (SpinNoPreempt) │    │
    │  │  DISPLAY_DEVICES(SpinNoPreempt) │    │
    │  │  INPUT_DEVICES  (SpinNoPreempt) │    │
    │  │  VSOCK_DEVICES  (SpinNoPreempt) │    │
    │  │  VIRTIO_9P_DEVICES(SpinNoPreempt)│   │
    │  └─────────────┬───────────────────┘    │
    │                │                        │
    │  ┌─────────────┴───────────────────┐    │
    │  │  ClassDevice<T>                 │    │
    │  │   Arc<ClassDeviceInner<T>>      │    │
    │  │   ├─ parent: Arc<DeviceObject>  │    │
    │  │   ├─ runtime: T (Box<dyn Trait>)│    │
    │  │   ├─ name, device_kind, irq     │    │
    │  │   └─ metadata (input identity)  │    │
    │  └─────────────────────────────────┘    │
    │                                         │
    │  Delegation impls:                      │
    │   ClassDevice<NI> : NetDevice           │
    │   ClassDevice<CI> : CharDevice          │
    │   ClassDevice<DI> : DisplayDevice       │
    │   ClassDevice<II> : InputDevice         │
    │   ClassDevice<VI> : VsockDevice         │
    │   ClassDevice<9I> : Virtio9pDevice      │
    └──────────────────┬─────────────────────┘
                       │
                       │ query / subscribe
                       v
         knet / fs_boot / input subsystem / ...
```

| 组件 | 职责 |
|------|------|
| `ClassDevice<T>` | 类型化的运行时设备句柄；包装 `DeviceObject` + trait object，委托 trait 方法调用 |
| `ClassDeviceInner<T>` | `ClassDevice` 的内部共享状态：parent、runtime、name、kind、irq、metadata |
| `ClassRegistry<T>` | 类型化注册表：publish（唯一发布）、devices（枚举活跃设备）、find（按 ID 查找）、subscribe（订阅可用性）、remove（按 ID 移除） |
| `ACTIVATION_BRIDGE` | 全局事件桥接：订阅 `kdevice` 的 `Activated` / `Removed` 事件，驱动 class 级别的 notify/remove |
| `class_registries!` 宏 | 声明式批量生成 7 个 class 注册表及其所有配套函数 |
| `ClassDeviceMetadata` | 可选的 class 特定元数据（当前仅 input class 携带 physical_location / unique_id） |
| `prelude` 模块 | 集中 re-export 所有公开类型，方便 `kdriver` 等发布者统一导入 |
| Trait delegation impls | 为需要 class handle 调用的类别实现操作 trait（如 `NetDevice`），委托到 inner runtime；net class 同时转发 `NetRxScheduler` attach/detach，block I/O 只经 block core canonical `BlockDevice` |

## 状态机

### ClassDevice 生命周期

```text
                    driver probe_device()
                         │
                         │ publish_<class>(parent, runtime)
                         v
                    ┌──────────┐
                    │Published │  ← ClassDevice 创建，注册表持有
                    │(pending) │     parent.state != Active
                    └────┬─────┘
                         │ kdevice Activated event
                         ▼
                    ┌──────────┐
                    │Available │  ← parent.state == Active
                    │(active)  │     触发 availability callbacks
                    └────┬─────┘
                         │ kdevice Removed event
                         ▼
                    ┌──────────┐
                    │ Removed  │  ← 从注册表中 swap_remove
                    └──────────┘
```

| 从 | 到 | 触发条件 |
|----|----|----------|
| — | Published | 驱动调用 `publish_<class>(parent, runtime)` |
| Published | Available | `kdevice` 分发 `Activated` 事件，`notify_class_available` 调用 subscriber callbacks |
| Published | Available (immediate) | publish 时 `parent.state()` 已经为 `Active`（desc-adoption 路径） |
| Available | Removed | `kdevice` 分发 `Removed` 事件，`remove_class_device` 从注册表移除 |
| Published | — | 同一 `DeviceId` 再次 publish 返回 `AlreadyExists` |

### 注册表 publish 语义

`ClassRegistry::publish` 与 Linux device registration 一样拒绝重复 identity。热插拔重注册
必须先经过 `Removed` lifecycle 删除旧对象，再发布新对象，不能静默替换 resident runtime。

## 算法流程

### publish 流程

1. 驱动在 `DeviceDriver::probe_device` 中创建运行时设备（如 `VirtIoNet::try_new`）。
2. 调用 `publish_<class>(parent, runtime)`（macro-generated）。
3. 从 runtime 提取 `name()`、`device_kind()`、`irq()`。
4. 校验 `device_kind` 与注册表类型匹配，不匹配时返回 `InvalidInput`。
5. 从 runtime 提取 `class_metadata()`（仅 input class 返回有效元数据）。
6. 构造 `ClassDevice::try_new_with_class_metadata`：
   - 验证 parent 有 `driver_name()` 和 `driver_id()`（必须已绑定驱动），否则返回 `BadState`。
7. 注册表 publish（`SpinNoPreempt` 锁内）：
   - 同一 ID 已存在 → `AlreadyExists`，撤销已经执行的 class-specific publish
   - 否则 → push
8. 如果设备已处于 `Active` 状态，同步触发 `notify_class_available`，通知所有已注册 subscriber。

### 设备枚举与查找

1. `*_devices()`：获取注册表锁，遍历所有条目，过滤 `is_available()`（parent.state == Active），克隆返回。
2. `find_*_device(id)`：获取注册表锁，查找匹配 ID 且 `is_available()` 的条目。
3. 这两个操作都返回 `ClassDevice<T>` 的克隆（`Arc` 共享），调用者可安全地跨锁边界使用。

### 可用性订阅

1. 子系统在初始化阶段调用 `subscribe_<class>_available(callback)`。
2. callback 以 `ClassAvailabilityCallback<T> = Arc<dyn Fn(ClassDevice<T>) + Send + Sync>` 形式存储。
3. 当设备变为 `Active` 时，`notify_class_available` 在锁外调用所有 subscriber callback。
4. subscriber 在 callback 中可选择立即使用设备或缓存引用。
5. callback 调用在锁外执行，避免 subscriber 回调中的重入导致死锁。

### 事件桥接

`ACTIVATION_BRIDGE` 在首次 class registry 访问时惰性初始化（`ensure_event_bridge`）：

1. 注册 `DeviceEventKind::Activated` subscriber：收到事件后调用 `notify_class_available(kind, id)`，
   按 `DeviceKind` 分发到对应 class 的 notify 函数。
2. 注册 `DeviceEventKind::Removed` subscriber：收到事件后调用 `remove_class_device(id)`，
   遍历所有 class 注册表执行 `remove`。
3. 桥接在 `LazyInit::call_once` 中完成，保证只注册一次。

### Device trait 委托

需要由消费者直接持有 class handle 的 `ClassDevice<T>` 实现对应操作 trait，委托到 inner
runtime。block class 是生命周期发布入口；I/O 消费者从 block core 取得 canonical
`BlockDevice`，因此不再为 `ClassDevice<BlockDeviceImpl>` 重复实现整套 block operations。

```rust
impl NetDevice for ClassDevice<NetDeviceImpl> {
    fn can_tx(&self) -> bool {
        self.with(|device| device.can_tx())
    }
    // ...
}
```

`with()` 方法通过 `&self.inner.runtime` 共享借用 runtime，不持有锁。
runtime 内部的并发控制由驱动自行负责（通常通过 interior mutability）。

### Display 设备的 framebuffer 路径

`DisplayDevice` trait 只暴露分辨率 (`DisplayInfo { width, height }`) 与 scanout
resource 接口（`create_scanout_resource` / `destroy_scanout_resource` /
`present_scanout_resource`）。kclass 的 class adapter 对这些方法做纯委托，
不持有任何 framebuffer 裸指针或直接内存映射。

`/dev/fb0` 的 framebuffer 兼容层由 `fbdevice` crate 实现：它在 `fb_init` 时向主显示
设备分配一块 shadow buffer，通过 `create_scanout_resource` 绑定为 host 可见的 2D
resource，再按需通过 `fb_present` 推到 scanout（无后台刷新任务：持续
`present_scanout_resource` 会与 DRM 合成器竞争单一物理 scanout 造成闪烁，因此
`/dev/fb0` 的写入与 `FBIOPAN_DISPLAY` ioctl 才触发呈现）。这套
"fbdev emulation over scanout" 模型对任何 `DisplayDevice` 统一适用，无需驱动自行暴露
直接映射的 framebuffer，因此 kclass 不再需要 framebuffer 特殊处理或 unsafe 边界。

## 并发模型

- 每个 class 注册表由 `SpinNoPreempt<ClassRegistry<T>>` 保护：
  publish / devices / find / subscribe / remove 操作在锁内完成。
- `notify_class_available` 采用「锁内查找设备 + 锁外回调」模式：
  - 在锁内查找设备并克隆 subscriber 列表；
  - 在锁外遍历 subscriber 执行 callback。
  这避免了 subscriber 回调中的重入导致死锁。
- `ACTIVATION_BRIDGE` 的初始化由 `LazyInit` 保证线程安全。
- `ClassDevice<T>` 通过 `Arc` 共享，`Clone` 仅增加引用计数。
- runtime (trait object) 的并发安全由 `Box<dyn Trait + Send + Sync>` 的 Sync bound
  和各驱动内部的 interior locking 保证。

## 设计决策

### 宏驱动而非手写每个 class

**选择**：使用 `class_registries!` 宏批量生成 7 个设备 class 的所有代码。

**Trade-off**：宏的调试信息较差，但换取以下好处：
- 每个 class 的 publish/devices/find/subscribe/notify/remove 逻辑完全一致，
  手写 7 份会有大量重复代码；
- 增加新 class 只需在 `class_registries!` 调用点添加一行参数，
  无需编写重复的样板代码；
- 宏展开后的代码与手写代码完全等价，无运行时开销。

**拒绝的方案**：手写每个 class 的完整代码。重复度高，新增 class 容易遗漏步骤
（如忘记实现 notify 或 ensure_event_bridge 调用），宏保证了一致性。

### Device trait 委托而非暴露 trait object

**选择**：为 `ClassDevice<T>` 实现每个操作 trait，内部通过 `with()` 委托。

**Trade-off**：每个 class 需要写 5-10 个方法的委托 impl，但换取以下好处：
- 上层子系统使用 `ClassDevice<NetDeviceImpl>` 与使用 `&dyn NetDevice` 的体验一致；
- `ClassDevice` 在委托调用前后可插入通用逻辑（如统计、日志、权限检查），
  无需修改各驱动实现；
- 封装了 `DeviceObject` 的生命周期管理，上层子系统无需关心设备状态。

**拒绝的方案**：暴露 `fn runtime(&self) -> &T` 让调用者直接操作。
该方案简化了委托代码，但破坏了封装——调用者可能 bypass 设备状态检查。

### 事件桥接惰性初始化

**选择**：使用 `LazyInit` + `ensure_event_bridge()` 在首次 class registry 访问时注册事件监听。

**Trade-off**：首次 registry 访问有微小的初始化开销，但换取以下好处：
- 不需要在 `init_drivers` 中显式调用 kclass 初始化；
- kclass 的使用者无需关心初始化顺序——只要 kdevice 已初始化即可；
- 如果编译配置未启用任何 class feature，事件桥接不会被注册，节省资源。

**拒绝的方案**：在 `kclass` 的全局构造函数中（如 `LazyInit` of a dummy）注册事件。
该方案更"主动"但引入了隐式的初始化依赖——kdevice 在构造 kclass 时必须已初始化。

### Input class 携带运行时身份元数据

**选择**：仅 input class 通过 `ClassDeviceMetadata` 携带 `physical_location` 和 `unique_id`。

**Trade-off**：`ClassDeviceMetadata` 目前 90% 时间是空的，但换取以下好处：
- input 子系统的设备匹配（evdev device identity）需要这两个字段；
- `ClassRuntimeMetadata` trait 的默认实现返回空元数据，
  其他 class 无需任何代码即可工作；
- 未来其他 class 如需携带元数据，只需覆盖 `class_metadata()` 方法。

**拒绝的方案**：把 `physical_location` / `unique_id` 加入 `Device` trait。
该方案会将 input 特定的概念污染通用设备抽象。

### `Arc<DeviceObject>` 防止 use-after-remove

**选择**：`ClassDevice` 持有 `Arc<DeviceObject>` 强引用。

**Trade-off**：即使设备已从 class 注册表 remove，外部持有的 `ClassDevice` 克隆仍能访问其 trait 方法。
这对于以下场景是必要的：
- poller / async task 持有的设备引用在设备热移除后仍需完成最后一次 I/O；
- subscriber callback 中获取的引用可能在设备 remove 后仍被使用。

**拒绝的方案**：`ClassDevice` 持有 `Weak<DeviceObject>`。
该方案允许 `DeviceObject` 在 remove 后立即释放，
但要求每次 `with()` 调用前都 upgrade weak pointer，
增加失败路径（设备已释放时返回错误）。

## Drop / 资源释放

- `ClassDevice` 的 drop 仅减少 `Arc` 引用计数，不触发任何设备操作。
- 注册表 remove 仅执行 `Vec::swap_remove`，移除 `ClassDevice` 条目。
- 当最后一个 `ClassDevice` 和 `DeviceObject` 引用都释放后，
  `DeviceObject` 的 devres（MMIO 映射、IRQ、DMA buffer）按 LIFO 顺序清理。
- runtime trait object 的 drop 由 `Box<T>` 负责，在 `ClassDeviceInner` drop 时执行。

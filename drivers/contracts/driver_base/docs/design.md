# driver_base — 设计文档

## 定位

本模块提供 x-kernel 所有设备驱动的公共基础接口，定义了设备分类枚举
`DeviceKind`、驱动错误类型 `DriverError` / `DriverResult`，以及所有设备
必须实现的 `Device` trait。它是 `drivers/` 下各子 crate
（block、net、display、input、vsock、virtio、kdriver）的共同依赖，
为内核驱动框架提供统一的类型契约。

## 背景

x-kernel 运行在裸机 `no_std` 环境中，无法使用 `std::io::Error` 等标准库
错误类型。同时，内核需要对异构设备（块设备、网络设备、显示设备等）进行
统一管理，因此需要一套轻量、无依赖的公共接口层，使各驱动 crate 在错误
处理和设备身份描述上保持一致。

## 范围

涉及的源文件：

```
driver_base/
├── src/
│   └── lib.rs
│
└── Cargo.toml
```

## 架构

```
┌──────────────────────────────────────────────────────────────┐
│                    driver_base                               │
│                                                              │
│  DeviceKind ──分类──> Device                                 │
│       │                │                                     │
│       │                ├── name()                            │
│       │                ├── device_kind()                     │
│       │                └── irq()                             │
│       │                                                      │
│  DriverError ──错误──> DriverResult<T>                       │
│       │                │                                     │
│       ├── should_retry()                                     │
│       └── message()                                          │
└──────────────────────────────────────────────────────────────┘
        ▲            ▲            ▲            ▲
        │            │            │            │
   block crate   net crate   display crate  virtio crate
   (及 kdriver、input、vsock 等所有驱动子 crate)
```

| 组件 | 职责 |
|------|------|
| `DeviceKind` | 枚举所有支持的设备类别（9 种），提供 `as_str()` 稳定短名 |
| `DriverError` | 统一驱动错误码，提供重试判断和日志消息 |
| `DriverResult<T>` | 驱动操作的专用 `Result` 类型别名 |
| `Device` | 所有设备必须实现的 trait，定义设备身份元数据接口 |

## 状态机

本模块为纯类型定义，无状态管理，不涉及状态机。

## 算法流程

本模块无复杂算法。核心逻辑仅为枚举匹配：

### 错误重试判断

1. 调用者收到 `DriverResult::Err(e)`
2. 调用 `e.should_retry()` 判断是否为可重试错误（`WouldBlock` / `ResourceBusy`）
3. 若可重试，调用者按自身策略进行退避重试

### 设备分类查询

1. 通过 `Device::device_kind()` 获取 `DeviceKind`
2. 按 `DeviceKind` 变体分发到对应子系统处理

## 并发模型

本模块仅定义类型和 trait，无内部可变状态，无并发问题。

- `Device` 要求实现者满足 `Send + Sync`，确保 trait object 可跨线程共享。
- `DeviceKind` 和 `DriverError` 均为 `Copy` 类型，天然线程安全。

## 设计决策

### 为什么用枚举而非字符串表示设备类别

使用 `#[repr(u8)]` 枚举而非字符串：
- 编译期穷举匹配，遗漏变体时编译器报错
- 零堆分配，`Copy` 语义，适合热路径
- `as_str()` 提供人类可读名称，仅在日志/调试时使用

### 为什么 DriverError 不实现 std::error::Error

x-kernel 为 `no_std` 环境，无法依赖 `std::error::Error` trait。
通过实现 `core::fmt::Display` 提供基本的错误描述能力。

### 为什么 Device trait 只定义三个方法

`Device` 故意保持最小接口，仅描述设备身份元数据：
- `name()` / `device_kind()` / `irq()` 是所有设备都需要的身份信息
- 具体操作（读写、配置等）由各子 crate 的专用 trait 定义，以 `Device` 为 super-trait
- 避免在基础层引入不必要的抽象，保持正交性

### 为什么 irq() 返回 Option<usize> 而非 Result

部分设备（如 ramdisk）不使用中断，`Option` 语义更准确：
- `None` 表示设备不使用中断（正常情况）
- `Some(irq)` 表示设备使用指定中断号
- `Result` 的 `Err` 语义暗示操作失败，不适用于"无中断"的场景

### 为什么 DeviceKind 包含 Bus 变体

总线控制器（如 PCI 主桥）本身也是需要被驱动框架管理的设备：
- 总线驱动负责枚举和配置子设备
- 统一纳入 `DeviceKind` 可复用设备注册和发现机制
- 与其他设备类别享有相同的身份查询接口

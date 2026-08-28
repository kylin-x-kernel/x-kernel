# driver_base — 安全与可靠性分析

## 信任模型

```
驱动子 crate（block、net、display、input、vsock、virtio、kdriver）
   │
   │ safe API: DeviceKind, DriverError, DriverResult, Device
   │
   v
┌──────────────────────────┐
│  driver_base             │
│                          │
│  ┌── unsafe 边界 ──────┐ │
│  │ （无 unsafe 代码）   │ │
│  └─────────────────────┘ │
└──────────────────────────┘
```

- **safe API 调用者**：模块仅提供纯 safe 类型定义和 trait，调用者无需额外证明安全性。
- **unsafe API 调用者**：无 unsafe API。

## unsafe 代码清单

本模块不含任何 `unsafe` 块、`unsafe fn` 或 `unsafe impl`。

## 内存安全不变量

本模块无 `unsafe` 代码，无需维护额外的内存安全不变量。
所有类型均为 `Copy` 或纯 trait 定义，不存在堆分配或裸指针操作。

## 线程安全

| 类型 | `Send` 条件 | `Sync` 条件 |
|------|-------------|-------------|
| `DeviceKind` | 自动 `Send`（`u8` 枚举，`Copy`） | 自动 `Sync`（`u8` 枚举，`Copy`） |
| `DriverError` | 自动 `Send`（`Copy` 枚举） | 自动 `Sync`（`Copy` 枚举） |
| `DriverResult<T>` | 当 `T: Send` 时 `Send` | 当 `T: Sync` 时 `Sync` |
| `Device` | trait 约束 `Send + Sync` | trait 约束 `Send + Sync` |

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | `Device` 实现者返回不一致的 `DeviceKind`，导致设备被错误子系统管理 | 中 | 驱动实现 bug，`device_kind()` 返回值与实际设备类型不符 | 各子 crate 在注册设备时校验 `DeviceKind` 与 trait 一致性；代码审查时重点检查 |
| T-02 | `Device::name()` 返回空字符串或无效 UTF-8 引用 | 低 | 驱动实现 bug | `name()` 返回 `&str`，Rust 保证 UTF-8 合法性；空字符串不影响安全，仅影响日志可读性 |
| T-03 | `DriverError` 新增变体后调用方未穷举处理 | 低 | 修改 `DriverError` 枚举但未更新所有 `match` | `match` 穷举检查由编译器强制执行；`should_retry()` 和 `message()` 为集中处理点，新增变体时必须更新 |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | `Device` 实现返回错误的 `DeviceKind` | 驱动实现者误写 `device_kind()` 返回值 | 设备被路由到错误子系统 | 设备不可用，但不会导致内存安全问题 | 3 | 子系统注册时校验；代码审查 |
| F-02 | `irq()` 返回错误的中断号 | 驱动实现者误写中断号 | 中断路由到错误处理程序 | 可能导致设备中断丢失或错误处理 | 2 | 中断注册框架应校验 IRQ 号合法性 |
| F-03 | `should_retry()` 对新增错误变体返回 false | 新增 `DriverError` 变体后未更新 `should_retry()` | 可重试错误被当作永久失败 | 非阻塞操作提前失败 | 4 | 编译器穷举检查强制更新 `match` |
| F-04 | `message()` 对新增错误变体返回错误描述 | 新增 `DriverError` 变体后未更新 `message()` | 日志信息不准确 | 调试困难，无安全影响 | 4 | 编译器穷举检查强制更新 `match` |

## 故障管理

- **错误码**：`DriverError` 枚举覆盖常见驱动故障场景，各变体语义明确。
- **Panic 策略**：本模块无 panic 路径。
- **故障恢复**：`should_retry()` 为调用者提供重试判断依据，具体恢复策略由调用者决定。

## 隐私分析

本模块不直接处理用户数据，不涉及隐私问题。

## 已知限制

1. `DriverError` 为固定枚举，无法携带附加上下文信息（如底层错误码、偏移量等），
   调用者如需详细错误信息需通过其他机制传递。
2. `Device::irq()` 返回 `Option<usize>`，不支持多个中断号的设备
   （如多队列网卡），后续可能需要扩展为返回中断号切片。

## 审计清单

修改本模块时需验证：

- [ ] 每个 `unsafe` 块均有 `SAFETY:` 注释（当前无 unsafe 代码）
- [ ] 新增 `DeviceKind` 变体后所有 `match` 穷举已更新
- [ ] 新增 `DriverError` 变体后 `should_retry()` 和 `message()` 已更新
- [ ] `Device` trait 变更为 breaking change，需同步所有实现者
- [ ] 新增 panic 路径有对应的 PanicGuard 或等效保护（当前无 panic 路径）

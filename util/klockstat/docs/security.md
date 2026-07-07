# klockstat — 安全与可靠性分析

## 概述

`klockstat` 是只增型诊断模块：在既有锁路径上追加原子计数，不改变锁语义。
主要风险在于信息暴露、统计精度与 `unsafe` 注册表访问边界。

## 信任模型

```
用户态 reader (/proc/lock_stat)
        │
        │ 只读文本
        v
┌─────────────────────────────┐
│  dump_lock_stat / snapshot  │
└─────────────────────────────┘
        │
        │ Relaxed 原子读
        v
┌─────────────────────────────┐
│  LockClassStats (per class)  │ <── 内核锁 fast path 写入
└─────────────────────────────┘
```

- **锁实现（ksync/kspin）** 信任 `LockClassStats::record_*` 为无副作用计数。
- **用户态读者** 仅能获得聚合统计，不能通过本接口修改内核状态。

## 外部边界 / 攻击面

| 边界 | 说明 | 风险 |
|------|------|------|
| `/proc/lock_stat` | 向有 procfs 访问权限的进程暴露文本 | 泄露内核源文件路径（`file:line`），辅助定位内核布局 |
| 统计计数本身 | 可被侧信道间接观测（读延迟） | 低；与常规性能计数类似 |

本模块：

- **不直接访问用户内存**；
- **不解析用户输入**（输出为内核生成的固定格式文本）；
- **不改变锁持有顺序或唤醒语义**。

启用 `KFEAT_LOCK_STAT` 时应在威胁模型中记录：`location` 字段暴露编译进镜像的
源路径字符串。

## Unsafe 清单

| 位置 | 操作 | 不变量 |
|------|------|--------|
| `runtime.rs` `RegistryLock::with` | `UnsafeCell` 可变借用 | `locked` 原子位串行化独占访问 |
| `RegistryLock` | `unsafe impl Sync` | 所有可变访问经 `with` 串行化 |

`linkme` 分布式切片由链接脚本 `KEEP` 收集，依赖构建系统正确保留
`LOCK_CLASSES` 段。

## 线程安全

- `LockClassStats` 计数器为 `AtomicU64`，多 CPU 并发 `record_*` 安全。
- 运行时注册表在首次注册时写入，之后主要为快照读取；`with` 保证互斥。
- 快照使用 `Relaxed` load，允许轻微撕裂（诊断可接受）。

## 失败与滥用场景

| 场景 | 影响 | 缓解 |
|------|------|------|
| 关闭 `KFEAT_LOCK_STAT` 仍挂载 procfs 节点 | 旧行为：空表误导 | 已通过 `procfs/lock_stat` feature 门控节点 |
| 大量唯一 `Mutex::new` 调用点 | 运行时注册表与泄漏字符串增长 | 按调用点去重；仅诊断配置启用 |
| 误用 `static Mutex::new`（stats 开） | 编译失败 | 文档要求使用 `static_lock!` |
| 计数与真实竞争不完全一致 | 误判热点 | `design.md` 明确指标语义 |

## 建议

- 生产镜像默认关闭 `KFEAT_LOCK_STAT`。
- 限制 procfs 挂载与读取权限（沿用现有 procfs 访问控制）。
- 勿在 IRQ 上下文首次调用 stats 模式下的 `Mutex::new`。

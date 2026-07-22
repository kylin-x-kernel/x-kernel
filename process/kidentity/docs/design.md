# kidentity — 设计文档

## 定位

`kidentity` 是 process domain 内的 PID/TID 身份号空间 owner。
它为 `kns`、`kprocess`、`ktask` 和 `posix/process`
提供 PID namespace 与 `PidHandle` 分配语义，
并集中维护 “identity 在 runnable 之前已经稳定发布” 所依赖的编号抽象。

## 背景

PID/TID 编号虽然被多个子系统消费，
但它们共享的是同一套 process-domain 语义：

- namespace 层级；
- 每层 namespace 内的可见编号；
- root-visible 编号回退；
- 线程/进程 identity 的稳定句柄。

把这部分从 `util/` 调整到 `process/`
是为了让 crate 归属与真实 ownership 一致，
避免把 process 语义误建模成通用工具层。

## 范围

涉及的源文件：

```text
process/kidentity/
├── Cargo.toml
├── docs/
│   ├── design.md
│   └── security.md
└── src/
    └── lib.rs
```

## 架构

```text
PidNamespace
  ├─ parent: Option<Arc<PidNamespace>>
  ├─ level: u32
  └─ next_nr: AtomicU32
          │
          v
PidHandle
  └─ numbers: Vec<Upid>
                 ├─ nr
                 └─ ns
```

| 组件 | 职责 |
|------|------|
| `PidNamespace` | 表示一层 PID namespace，并维护本层下一个可分配编号 |
| `Upid` | 表示某个 namespace 中可见的单层编号 |
| `PidHandle` | 聚合当前 namespace 到 root namespace 的编号链，作为稳定 task/process identity |
| `ROOT_PID_NS` | 提供全局共享的 root PID namespace |

## 调用约束 / 执行上下文

- 该 crate 不依赖 current task、地址空间或调度器状态。
- API 可在早期初始化阶段使用，只要求堆分配与原子操作可用。
- 当前实现不阻塞，不睡眠，也不访问用户内存。
- 调用方必须把 `PidHandle` 的发布顺序与更高层 task/process 可见性顺序配合好；
  `kidentity` 只负责分配，不负责 publication 事务。

## 算法流程

### `PidHandle::allocate_in`

1. 从 `active_ns` 沿 `parent` 链回溯到 root，构造 namespace 链。
2. 对链上的每一层 namespace 递增 `next_nr`，生成对应 `Upid`。
3. 按从当前 namespace 到 root 的顺序保存到 `PidHandle::numbers`。

### `PidHandle::nr_in`

1. 在线性 `numbers` 向量中查找目标 namespace。
2. 如果命中，返回该层编号。
3. 如果未命中且目标是 root namespace，则回退到 `root_nr()`。

## 并发模型

- `PidNamespace::next_nr` 使用 `AtomicU32`，
  允许多个分配路径并发获取新编号。
- namespace 层级结构在构造后不可变，
  通过 `Arc<PidNamespace>` 共享。
- `PidHandle` 在创建后只读，不支持编号回收或重写。
- 当前 `nr_in()` 为线性扫描；
  这是基于 namespace 深度通常很浅的假设。

## 设计决策

### 保持 handle 与 namespace 分离

`PidNamespace` 拥有编号空间，
`PidHandle` 只保存一次分配结果。
这样可以把 “空间所有权” 与 “对象稳定身份” 分开，
避免把进程生命周期管理耦合进底层编号器。

### 当前不做 PID reuse

当前实现只递增 `next_nr`，
溢出时返回 `KError::WouldBlock`。
这保留了简单、可审计的 publish-before-runnable 模型；
后续如果引入 PID reuse，
也应维持 identity 在对外可见前已经稳定这一不变量。

### PID 1 由 boot lifecycle 保证

root namespace 的普通分配器从 1 开始。boot、idle、late-init 和普通内核
worker 都使用 PID-less identity，因此 `SystemInitEntry` 创建 init 时的第一笔
普通分配必须得到 PID 1。后续 PID 不承载启动期固定角色，按正常分配顺序产生。
如果未来某条早期路径在 init 创建前启动 Linux-visible task，init 侧的 PID 1
断言会暴露启动顺序破坏；`kidentity` 本身不保存 init 专用全局 handle。

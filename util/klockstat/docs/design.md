# klockstat — 设计文档

## 定位

`klockstat` 为 X-Kernel 提供按 lock class 聚合的锁竞争统计，供内核诊断
热点锁与阻塞路径。统计结果通过 `/proc/lock_stat`（Kconfig `KFEAT_LOCK_STAT`）
或 `klockstat::dump_lock_stat()` 读取。

## 范围

```
util/klockstat/
├── src/
│   ├── lib.rs       # 计数器、聚合、格式化输出
│   └── runtime.rs   # 堆锁按初始化调用点注册（stats feature）
util/klockstat-macros/
└── src/static_lock.rs  # static_lock! 过程宏
```

集成点：

- `ksync::Mutex` / `ksync::RwLock`
- `kspin::SpinLock` 家族（`SpinNoIrq` 等）

## Host Test Layout

Host unit tests live in `tests/host/` and remain private path modules in
their original parents, not separately discovered Cargo integration targets.
`tests/host/lib.rs` preserves `tests` under `cfg(test)` (four cases);
`tests/host/runtime.rs` preserves `runtime::tests` under
`cfg(all(test, feature = "stats"))` (five cases). There are no guest tests.
The runtime test still compares two lookups from the same call site; it does
not depend on a fixed source filename or line number.

Validate on a host target with `cargo test -p klockstat --lib` and
`cargo test -p klockstat --features stats --lib` (four and nine cases).
Use host-target Cargo settings without kernel `--cfg unittest`.

## Lock Class 模型

每个 lock class 由 `(location, kind)` 标识：

| 字段 | 含义 |
|------|------|
| `location` | 通常为 `file:line` |
| `kind` | `Mutex`、`RwLock`、`SpinNoIrq` 等 |

同一 class 下的多个锁实例共享一组计数器。

### 注册路径

**静态锁** — 使用 `static_lock!`：

```rust
use ksync::{Mutex, static_lock};

static_lock! {
    static TASK_TABLE: RwLock<TaskTable> = RwLock::new(TaskTable::new());
}
```

宏展开为：

1. 独立的 `LockClassStats` 静态项；
2. `linkme::distributed_slice(LOCK_CLASSES)` 条目；
3. 绑定 `new_with_stats` 的锁静态变量。

**堆锁 / 非常量静态上下文** — `Mutex::new` / `RwLock::new`（`stats` 开启时）：

- 构造函数带 `#[track_caller]`；
- 首次调用时通过 `class_for_init_site` 在运行时注册表按调用点去重；
- 与静态路径一样按 `file:line` 聚合。

**未追踪锁** — 指向 `NOOP_CLASS`：

- `RawMutex::with_config` 等未绑定 stats 的 raw 锁；
- `Mutex::const_new(RawMutex::new(), ...)` 用于必须 `const` 初始化的场景。

**`klazy::lazy_static!` 中的锁** — 保持 `lazy_static` 不变时：

- `stats` 开启后仍可通过运行时 `Mutex::new` 追踪；
- `location` 落在初始化函数/闭包内部，而非 `static ref` 声明行；
- 需要精确标签时应改用 `static_lock!`。

## 指标语义

| 指标 | 含义 |
|------|------|
| `acquisitions` | 成功获取锁的次数（`lock()` 成功路径与 `try_lock` 成功） |
| `contentions` | 获取前发生等待的次数 |

补充约定（对齐 Linux `lock_contended` / `lock_acquired` 模型）：

- **Mutex**：仅在确认需要睡眠、进入 `block_on` 之前记 1 次 `contentions`（每轮 `lock()`
  至多一次）；纯自旋后 CAS 成功不算 contention；成功时记 `acquisitions`。
- **SpinLock（SMP）**：自旋等待后成功获取记 1 次 `contentions` + `acquisitions`；
  无竞争直接成功只记 `acquisitions`。
- **SpinLock（UP）**：仅记 `acquisitions`。
- **RwLock**：仅在确认需要睡眠、进入 `block_on` 之前记 1 次 `contentions`（每轮
  `lock_shared` / `lock_exclusive` 至多一次）；每个成功读锁/写锁各记 1 次 `acquisitions`。

## 输出格式

`dump_lock_stat()` 按 `contentions` 降序取前 `DUMP_TOP_N`（默认 5）条，过滤全零
条目，输出固定列宽表格。

数据源合并：

1. `linkme` 注册的静态 class（`LOCK_CLASSES`）；
2. `runtime.rs` 运行时注册表中的堆锁 class。

## Feature 门控

| Kconfig / Cargo | 效果 |
|-----------------|------|
| 默认（关闭） | `static_lock!` 为恒等宏；`Mutex::new` 保持 `const fn`；无 hook 开销 |
| `KFEAT_LOCK_STAT` | 启用 `ksync/stats`、`kspin/stats`、`procfs/lock_stat` |

`stats` 关闭时 `klockstat` 仍可被 `procfs` 链接，但无运行时注册与宏展开。

## 设计取舍

- **按调用点而非类型聚合**：与 Linux `lock_stat` 类似，便于定位源码位置；同一行多个
  `Mutex::new` 因 `column` 不同可能分为多个 class。
- **Relaxed 原子**：统计为诊断用途，不要求与锁状态严格 happens-before。
- Runtime registration uses a fixed 1024-slot open-addressed table. Each slot
  is published once through `klazy::Once`; no heap allocation, container growth,
  or label formatting occurs during registration. The key includes file, line,
  column and lock kind. Slots are never removed or reused.
  Hash lookup uses linear probing bounded by the pool capacity, not a scan on
  every counter update. Publication can wait for another CPU's initializer;
  this is not a bounded-latency or NMI-safe primitive.
- This follows Linux's fixed `lock_classes[MAX_LOCKDEP_KEYS]` pool and
  `register_lock_class` exhaustion policy (`kernel/locking/lockdep.c`): capacity
  exhaustion permanently disables diagnostics instead of failing lock creation.
  X-Kernel disables both static and runtime counter recording; it does not
  implement Linux's dependency graph or per-CPU lock-stat arrays. Existing
  references and snapshots remain valid. In-flight counter updates may finish.
- Runtime labels retain compiler-provided static `Location` metadata.
  `LockClassEntry::location` and `LockClassStats::location()` now expose
  `LockLocation` instead of a string reference. Formatting still prints
  `file:line`; static labels are unchanged. Formatting and snapshot vectors
  allocate only on the reporting path, which remains infallible on OOM.
- Snapshot iteration uses nonblocking `Once::get`, without holding a registry
  lock. A concurrent registration may be absent from a snapshot. The dump
  explicitly reports pool exhaustion rather than presenting frozen counters
  as live statistics. `statistics_enabled()` exposes the same state.

## 调用约束

- Registration requires no sleep or reclaim permission. Kernel registration
  saves and disables local IRQs until publication finishes, then restores the
  original state, including on unwind. Only another CPU can own an initializing
  slot during ordinary task/IRQ registration. NMI registration is unsupported:
  NMIs can interrupt a slot initializer and must not wait on that same slot.
  Initialization performs no callbacks, logging, allocation or scheduling.
- The table and slot storage exist only with `stats`; the capacity is a local
  diagnostic budget, not a claim that all possible lock sites fit. Exhaustion
  does not retry or re-enable counters until reboot. Host tests use isolated
  small tables for exhaustion and concurrent publication checks.
  A subprocess isolates the permanent disable transition while checking that
  previously registered static and runtime counters stop and the dump reports it.
- 静态锁应优先使用 `static_lock!`，避免 `stats` 下 `static X = Mutex::new(...)` 因
  非 `const fn` 导致编译失败。

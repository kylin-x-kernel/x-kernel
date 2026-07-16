# kfutex — 安全与可靠性分析

## 信任模型

```text
ksyscall / kprocess / posix-process
          │
          │ safe API: FutexKey, ProcessFutexState, FutexTable, WaitQueue
          ▼
┌──────────────────────────────────────────┐
│ kfutex                                   │
│                                          │
│  key construction for shared/private     │
│  process-private futex table             │
│  shared futex table cache                │
│  wait/wake/requeue queues                │
│                                          │
│  no raw user-memory dereference          │
└──────────────────────────────────────────┘
          │
          ▼
memspace / vmobj / ktask scheduler
```

- 调用者负责在 syscall 边界完成用户地址、访问权限、超时值和 errno 映射校验。
- `kfutex` 负责维护 futex key、table 路由、entry 生命周期和等待队列不变量。
- `kfutex` 不直接解引用用户地址，也不负责线程 robust-list 生命周期。

## 外部边界 / 攻击面

| 边界 | 来源 | 进入 `kfutex` 的形式 | 约束 |
|------|------|------------------------|------|
| 用户 futex 地址 | futex syscall、线程退出清理 | `usize` 地址，经 `FutexKey::new` 参与 key 构造 | 调用者负责地址有效性和访问权限；`kfutex` 不直接解引用 |
| 地址空间映射 | `memspace` | `MmSpace` 查询结果和 VMA backing identity | `kfutex` 依赖 `memspace` 正确区分 shared/file-backed 映射 |
| 共享对象 identity | shared anon / file-backed mapping | `VmObjectId` / `MappingIdentity` | identity 只用于哈希索引，不参与解引用 |
| 调度与阻塞 | `ktask` | wait/wake/requeue | 调用者必须保证执行上下文允许阻塞 |

## unsafe 代码清单

本 crate当前没有 `unsafe` 代码块、`unsafe fn` 或 `unsafe impl`。

## 内存安全不变量

1. `FutexKey` 的 shared identity 仅用于哈希索引，绝不用于解引用。
2. `FutexTable` 中的 entry 只有在没有外部 guard 且等待队列为空时才能回收。
3. `WaitQueue` 中 inactive waiter 会在唤醒或 drop 路径下被移除，不能长期残留并指向失效 waker。
4. `ProcessFutexState` 必须保证 private key 永远不会路由到 shared table。
5. `SharedFutexTables` 只能复用同一 shared identity 对应的 table。
6. 对 file-backed shared object，同一 inode-owned `Mapping` 必须稳定地产生同一 shared futex identity。

## 线程安全

| 类型或状态 | 并发保护 | 说明 |
|------------|----------|------|
| `WaitQueue::queue` | `SpinNoIrq<VecDeque<...>>` | wait/wake/requeue 串行化 |
| `FutexTable` | `Mutex<HashMap<...>>` | entry 创建、查找时的引用稳定性和回收串行化 |
| `SharedFutexTables` | `Mutex<HashMap<...>>` | shared table cache 创建和周期清理串行化 |
| `FutexEntry::owner_dead` | `AtomicBool` | robust 清理路径与正常 wait 路径共享状态 |

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | private/shared table 路由错误导致跨进程串扰 | 高 | `ProcessFutexState::table_for` 误把 private key 路由到 shared cache | private/shared 分支集中在 `kfutex` owner 内维护 |
| T-02 | shared futex identity 复用导致旧等待队列串扰 | 高 | shared object identity 被错误复用且 stale table 未清理 | shared cache 周期清理空且无外部引用 table；shared-anon 使用稳定 `VmObjectId`，file-backed 使用稳定 `MappingIdentity` |
| T-03 | wait condition 与入队竞态导致错误阻塞 | 中 | 条件在检查与入队之间变化 | `WaitFuture::poll` **先持队列锁入队**（push 不触碰用户内存），**再在锁外**求值条件；并发 `wake()` 经 `is_active` token + `Drop` 移除 + Path-A 重轮询保证不丢唤醒。条件求值不能放在抢占禁用的队列锁内——`condition` 可能访问用户内存并缺页，缺页处理需阻塞（`blocked_resched` 要求 `preempt_disable_count == 2`）|
| T-04 | inactive waiter 长期滞留导致 wake 计数失真 | 中 | caller drop/timeout 后未正确清理 | waiter token 在 drop 路径清除并从队列移除 |
| T-05 | entry 过早回收导致后续 wake 丢失 | 中 | table 在仍有外部 guard 或活跃 waiter 时删除 entry | `FutexGuard::drop` 同时检查强引用计数和 wait queue 空状态 |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | shared table cache 泄漏 | stale table 长期未达到 cleanup 阈值 | cache 增长 | 内存占用上升 | 3 | 每 100 次查找 retain；删除空且无外部引用项 |
| F-02 | wait queue 清理不完整 | inactive waiter 未被移除 | wake 需要扫描更多项 | futex 路径变慢 | 3 | `wake`/`is_empty`/drop 都会剔除 inactive waiter |
| F-03 | wrong-key routing | key 构造或路由逻辑错误 | wait/wake 命中错误队列 | futex 语义错误 | 2 | key 构造与路由统一由 `kfutex` owner 维护 |
| F-04 | owner-dead 标志未清除 | robust 清理后未复位 | 下一次 wait 错误返回 `EOWNERDEAD` | robust mutex 语义错误 | 2 | 上层在 wait 成功返回后立即 `swap(false)` |

严重度定义：

- 1：致命，内存破坏或未定义行为。
- 2：严重，futex/robust 语义明显错误。
- 3：一般，性能下降或缓存增长。

## 故障管理

- `wait_if`、`wake` 和 `requeue` 使用 `KResult` 或显式返回值表达失败与唤醒数量。
- shared table cache 清理是 best-effort；未命中阈值时允许暂时保留 stale entry。
- `FutexGuard::drop` 只在可证明 entry 空闲时清理，避免激进回收。

## 已知限制

- **`WaitQueue::queue` 是 `SpinNoIrq`（抢占+中断禁用），其锁保护区内不得调用任何可能缺页/阻塞的代码。** `wait_if` 的 `condition` 闭包可能访问用户内存（如 futex word），必须在锁外求值；否则缺页处理路径阻塞时 `preempt_disable_count` 超过 2，触发 `blocked_resched` 断言 panic（见 T-03）。
- shared table cleanup 使用固定 100 次查找阈值，不是精确或实时回收机制。
- shared-anon 与 file-backed 路径都使用稳定 `VmObjectId` / `MappingIdentity`。
- 本 crate 不负责 robust-list 链表遍历和 `clear_child_tid` 语义，那些线程生命周期逻辑仍在 `kprocess` / `posix-process`。

## 审计清单

- 新增 futex 状态时，确认它属于 `kfutex` owner，而不是散落到 `kprocess` 或 syscall adapter。
- 修改 `FutexKey::new` 时，重新审计 shared/file-backed 映射 identity 来源。
- 修改 `ProcessFutexState::table_for` 时，确认 private/shared 路由不被打破。
- 修改 shared table cache 时，确认 stale entry 清理不会删除仍被外部持有的 table。
- 修改 `FutexGuard::drop` 时，确认 entry 只会在无外部引用且 queue 为空时回收。

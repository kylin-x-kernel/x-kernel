# kfutex — 设计文档

## 定位

`kfutex` 提供 x-kernel 的 futex 数据与等待队列 owner。
它拥有 futex key、进程级 futex table、shared futex table 路由与复用策略，以及 wait/wake/requeue 所需的不变量。
syscall adapter、线程退出清理和 robust-list 逻辑通过本 crate 操作 futex 状态，但不拥有这些状态本身。

## 背景

futex 语义同时依赖：

- 按地址或共享映射 identity 构造的 `FutexKey`；
- 每个进程私有的 futex table；
- 跨进程共享映射复用的 shared futex table；
- wait/wake/requeue 的等待队列语义。

这些数据和策略需要一起演进。
如果只把 `FutexTable` 类型放在一个 crate，而把实例管理和 shared/private 路由留在别处，owner 边界会被切开，审计 shared/private 隔离会变得困难。

## 范围

涉及的源文件：

```text
process/kfutex/
├── Cargo.toml
├── docs/
│   ├── design.md
│   └── security.md
└── src/
    ├── key.rs
    ├── lib.rs
    ├── process_state.rs
    ├── table.rs
    └── wait_queue.rs
```

## 架构

```text
sys_futex / exit robust-list / clear-child-tid wake
                  │
                  ▼
              FutexKey
                  │
                  ▼
        ProcessFutexState::table_for()
           ├─ private_table
           └─ SHARED_FUTEX_TABLES[region identity]
                  │
                  ▼
              FutexTable
                  │
                  ▼
              FutexEntry
           ├─ WaitQueue
           └─ owner_dead
```

| 组件 | 职责 |
|------|------|
| `FutexKey` | 标识 private futex 或 shared futex 映射区域中的逻辑地址 |
| `ProcessFutexState` | 保存进程私有 futex table，并把 shared key 路由到全局 shared table cache |
| `SharedFutexTables` | 缓存共享映射对应的 futex table，并周期性清理 stale entry |
| `FutexTable` | 保存 `key -> FutexEntry` 映射 |
| `FutexEntry` | 持有 `WaitQueue` 和 `owner_dead` 状态 |
| `WaitQueue` | 管理 wait/wake/requeue 的阻塞队列 |

## 调用约束 / 执行上下文

- `FutexKey::new` 需要调用者提供已锁住并可安全遍历的地址空间快照。
- `ProcessFutexState::table_for` 可能获取全局 shared table 锁，不应从中断上下文调用。
- `WaitQueue::wait_if` 可能阻塞，只能在可睡眠的线程上下文调用。
- `wake`、`requeue` 和 `owner_dead` 标记可用于退出清理路径，但调用点仍应位于 task/runtime 路径，而不是硬中断上下文。

## 状态机

### FutexEntry 生命周期

```text
Absent
  → get_or_insert()
  → Live(entry in table)
  → no external guards && queue empty
  → removed on FutexGuard::drop
```

`FutexEntry` 不保存引用计数之外的独立生命周期状态。
当外部 guard 释放且等待队列为空时，table 会在 `FutexGuard::drop` 中回收条目。

## 算法流程

### futex key 构造

```text
FutexKey::new(aspace, address)
  ├─ shared anonymous/file-backed mapping → Shared { offset, region }
  └─ otherwise                             → Private { address }
```

shared key 的 identity 使用：

- shared-anon 路径：`memspace` 暴露的 `VmObjectId`；
- file-backed 路径：通过 `memspace` 暴露的 VMA backing 描述，使用 inode-owned `MappingIdentity`。

### table 选择

```text
ProcessFutexState::table_for(key)
  ├─ Private → process-private Arc<FutexTable>
  └─ Shared  → SHARED_FUTEX_TABLES.get_or_insert(region identity)
```

shared table cache 每 100 次查找做一次 retain，删除“无外部强引用且 table 已空”的 entry。

### wait / wake / requeue

```text
syscall / runtime cleanup
  → table.get_or_insert(key)
  → FutexEntry::wq.wait_if / wake / requeue
  → FutexGuard::drop() on scope exit
```

`wait_if` 先在持队列锁状态下重新检查条件，只有条件仍满足才真正入队。

## 并发模型

- `WaitQueue` 使用 `SpinNoIrq<VecDeque<Waiter>>` 保护等待队列。
- `FutexTable` 使用 `Mutex<HashMap<...>>` 串行化 entry 创建和回收。
- `SharedFutexTables` 使用全局 `Mutex` 串行化 shared table cache 创建和周期清理。
- `owner_dead` 使用 `AtomicBool` 让 robust-list 清理路径与正常 wait 路径共享状态。

## 设计决策

### 将 table 实例和路由策略收回 `kfutex`

`FutexTable`、`ProcessFutexState`、shared table cache 和 `FutexKey` 需要围绕同一组不变量演进：

- private/shared 隔离；
- shared identity 复用策略；
- stale table 清理；
- entry 生命周期与等待队列清理。

因此这部分必须由 `kfutex` 统一拥有，而不是把类型和实例管理拆到不同 crate。

### file-backed shared futex 绑定 inode-owned mapping

Linux 对 file-backed shared futex 的核心不是“哪个 `struct file` 打开的”，而是
“它属于哪个 `file->f_mapping` / `struct address_space`”。

因此 `kfutex` 使用 inode-owned `MappingIdentity` 作为 file-backed shared table
的 region identity，而不是把 open-file 实例或 runtime 私有句柄当作共享对象本体。

### robust-list 仍留在 `kprocess`

robust-list head 和 `clear_child_tid` 属于线程生命周期状态，不属于 futex 数据 owner。
`kfutex` 只承接等待队列和 table 组织；线程退出时由 `kprocess`/`posix-process` 调用 `kfutex` API 完成唤醒。

## Drop / 资源释放

- `FutexGuard::drop` 在 entry 没有外部强引用且等待队列为空时移除 table entry。
- `SharedFutexTables` 不主动销毁仍被外部持有的 table；只在周期清理时删除 stale cache entry。
- `ProcessFutexState` drop 后，私有 table 随 `Arc` 生命周期释放；shared table 是否保留由全局 cache 的外部引用计数和空表状态共同决定。

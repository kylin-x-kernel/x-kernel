# tipc-handle — 设计文档

## 定位

`tipc-handle` 是 Trusty IPC handle 所有权和事件聚合 crate。
它抽出 `Handle` trait、`HandleSet` 和 process-local `HandleTable`，
让 `process/kprocess` 可以保存每进程 TIPC handle table，
同时避免依赖完整 `tee/tipc` core。

`tee/tipc` 依赖本 crate 来让 port、channel、memref 实现统一 handle 接口；
`process/kprocess` 依赖本 crate 来在 `ProcessRuntime` 中挂接 `HandleTable`。

## 背景

Trusty IPC 以整数 handle 作为进程本地对象能力。
这些 handle 可以指向 port、channel、handle set 或 memref。
如果 `HandleTable` 留在 `tee/tipc`，`kprocess` 需要依赖完整 TIPC core；
如果 syscall adapter 又放入 `tee/tipc` 并依赖 `kprocess`，会形成循环依赖。

本 crate 把共享的 handle 状态边界独立出来：

- handle trait 和事件位；
- handle wait/cookie 存储；
- handle set 事件聚合；
- process-local integer id 到 `Arc<dyn Handle>` 的映射。

## 范围

```text
tee/tipc-handle/
├── Cargo.toml
├── docs/
│   ├── design.md
│   └── security.md
└── src/
    ├── lib.rs
    ├── handle.rs
    ├── handle_set.rs
    └── handle_table.rs
```

## 架构

```text
process/kprocess
    │ owns RwLock<HandleTable>
    ▼
┌────────────────────────────────────────────┐
│ tipc-handle                                │
│                                            │
│  Handle trait                              │
│    ├─ poll/register/close                  │
│    ├─ cookie                               │
│    └─ checked downcast via Any             │
│                                            │
│  HandleSet                                 │
│    └─ handle_id -> HandleSetEntry          │
│                                            │
│  HandleTable                               │
│    ├─ handle_id -> Arc<dyn Handle>         │
│    ├─ handle_set_ids                       │
│    └─ cached wait_any_snapshot             │
└────────────────────────────────────────────┘
    ▲
    │ implements Handle for TIPC objects
tee/tipc
```

| 组件 | 职责 |
|------|------|
| `Handle` | 统一 port、channel、handle set、memref 的事件、close、cookie 和 downcast 接口 |
| `HandleWaitState` | 保存等待者 `PollSet` 和 caller cookie，供具体 handle 嵌入 |
| `HandleSet` | 多 handle 事件聚合器，返回 process-local handle id、事件和注册 cookie |
| `HandleTable` | 进程本地 integer id 到 `Arc<dyn Handle>` 的映射，并在 close 时清理 handle set registration |

## 调用约束 / 执行上下文

- 本 crate 是 `no_std` 内核 crate，依赖 `alloc`、`kpoll`、`kspin`、`kerrno` 和 `smallvec`。
- `Handle::register` 接收短生命周期 `PollContext` 并返回注册错误；调用方需要位于
  调度器可用之后，并持有 `PollRegistrations` 跨越 `Pending`。
- `HandleSet` 使用 `SpinNoIrq` 保护 registration，适合短临界区，不适合在锁内执行阻塞操作。
- `HandleTable` 不内置锁，调用方通常把它放入 process-local `RwLock`。
- 本 crate 不要求当前进程线程；只有持有 `HandleTable` 的上层调用者才决定 process-local 归属。
- early boot 和中断上下文不应创建或操作会注册 waker 的 handle set。

## 状态机

### Handle table 生命周期

```text
Arc<dyn Handle>
  └─ uctx_handle_install()
      ▼
process-local integer id
  ├─ uctx_handle_get()
  ├─ wait_any_snapshot()
  ├─ register_wait_any_table_change()
  ├─ uctx_handle_uninstall()
  │    └─ detach_from_handle_sets(id)
  └─ uctx_handle_remove()
       ├─ uctx_handle_uninstall(id)
       └─ handle.close()
  └─ uctx_handle_close_all() during exec / last-thread exit
       ├─ remove all table ownership
       ├─ clear handle-set registrations
       └─ close every handle
```

handle id 只在一个 `HandleTable` 内有效。
删除 handle 时会先从同 table 内所有 `HandleSet` 移除该 id，
再关闭目标对象，避免 stale registration 继续报告已关闭对象。
`wait_any_snapshot()` 在 handle table 增删之间复用缓存的
`Arc<[(handle_id, handle)]>`，避免每次 wait-any poll 都重新分配并 clone
整张表；install/uninstall 会让缓存失效并唤醒已注册的 wait-any 等待者。

### Handle set registration

```text
Empty
  └─ Add(handle_id, handle, mask, cookie)
      ▼
Registered
  ├─ Modify(...)
  ├─ Delete(...)
  ├─ poll_one()
  └─ close()
      ▼
Empty
```

当前禁止把 `HandleSet` 注册进另一个 `HandleSet`，避免第一阶段引入循环检测。

## 算法流程

### 安装 handle

```text
uctx_handle_install(handle)
  → remember whether handle is HandleSet
  → scan integer id space from next_id
  → insert Arc<dyn Handle>
  → if HandleSet, remember id in handle_set_ids
  → update next_id
```

id 分配从 `next_id` 开始环形扫描。
完整扫描仍找不到空位时返回 `TooManyOpenFiles`。

### 删除 handle

```text
uctx_handle_remove(id)
  → uctx_handle_uninstall(id)
  → handle.close()
```

先 detach 再 close，避免 close 触发等待者后仍能从 handle set 看到 stale id。
`uctx_handle_uninstall` 只删除 id 并返回 `Arc<dyn Handle>`，
用于 syscall 回滚等已经临时安装 handle 但还不能关闭底层对象的路径。
普通 close 路径必须使用 `uctx_handle_remove`。

`uctx_handle_close_all` 用于 exec 和进程退出两个边界。TIPC handle 不属于
POSIX file descriptor table，也没有 inheritable 或 close-on-exec 标志，因此
新可执行文件不得继承旧映像的 port、channel、handle set 或 memref。该方法先使
整张表及其 wait-any snapshot 失效，再显式关闭每个对象；这保证 port 被取消
发布，channel peer 被通知断开。进程最后一个线程退出时，`do_exit` 在发布
process exit 之前经 `Process::close_all_tipc_handles` 调用同一方法，避免
父进程 `wait` 返回或设备 shutdown 时仍观察到已退出进程的存活 handle owner。

## 并发模型

- `HandleWaitState` 使用 `kpoll::PollSet` 保存等待者，`AtomicUsize` 保存 cookie。
- `HandleSet` 使用 `SpinNoIrq<BTreeMap<...>>` 串行化 add/delete/modify/poll。
- `HandleTable` 本身不是并发容器；外部锁决定读写并发策略。
- `Arc<dyn Handle>` 保存对象强引用，handle set registration 不依赖裸指针。

## 设计决策

- **独立 crate 而不是 `tipc` 内部模块**：`kprocess` 只需要 handle table，不应依赖 port/channel/registry/syscall adapter。
- **`HandleSet` 跟 `HandleTable` 一起下沉**：`HandleTable` 需要 close-detach handle set registration，二者共享同一所有权不变量。
- **trait object 而不是 enum**：handle 的具体实现分布在 `tipc` core，使用 `Arc<dyn Handle>` 可以让 handle table 不依赖具体 TIPC 对象类型。
- **禁止 handle set 嵌套**：第一阶段选择保守语义，避免循环检测和递归 poll。

## Drop / 资源释放

- `HandleTable::uctx_handle_remove` 调用目标 handle 的 `close`，但不拥有具体对象的 drop 细节。
- `HandleSet::close` 清空 registration 并通知等待者。
- `HandleWaitState` 不拥有外部资源，drop 时只释放内部等待集合和 cookie。

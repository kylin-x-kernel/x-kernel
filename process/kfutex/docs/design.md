# kfutex — 设计文档

## 定位

`kfutex` 是 non-PI futex 的并发语义 owner。它拥有 canonical key、全局固定
bucket、waiter 状态机，以及 wait/wake/requeue/wake-op 的线性化规则。
syscall ABI、timeout flag 解析和线程退出时的 robust-list 遍历仍由上层负责。

PI futex 不在当前阶段范围内；在调度器具备有效优先级与 donation graph 前，
不得把 PI 状态混入 non-PI bucket。

## 组件

```text
process/kfutex/src/
├── key.rs       canonical private/shared key
├── table.rs     static buckets and compound operations
├── waiter.rs    Arc waiter and checked routing state
├── wake_op.rs   FUTEX_WAKE_OP decode/arithmetic
└── lib.rs
```

```text
MmSpace::resolve_futex_backing
          |
          v
      FutexKey
          |
          v
global FutexTable[256]
          |
          v
SpinNoPreempt<VecDeque<Arc<FutexWaiter>>>
```

## Key contract

- `FUTEX_PRIVATE_FLAG` 生成 `(mm_id, virtual_address)`。
- 未带 private flag 的 private VMA 仍生成 `(mm_id, virtual_address)`。
- shared anon/file VMA 生成
  `(VmObjectId, backing_page_index, byte_offset_in_page)`。
- backing offset 由 `memspace` 根据 VMA split/trim 后的 metadata 计算；
  `kfutex` 不自行推导 VMA-relative offset。
- 地址必须按 `u32` 自然对齐并落入用户地址范围。

## Waiter lifecycle

```text
Init -> Queued -> Woken
               -> Cancelled
```

waiter 同时由等待 future 和 bucket 中的 `Arc` 持有。future drop 只通过
`Arc::ptr_eq` 删除自身，不保存 bucket 或队列元素的裸指针。

requeue 在同时持有 source/target bucket 锁时更新 waiter 的 key、bucket id 和
generation。取消路径先读取 route，再获取对应 bucket 锁并复核 generation；
若 route 已变化则重试。

signal 或 timeout 只能在持 bucket 锁成功执行 `Queued -> Cancelled` 后作为错误
返回。如果 WAKE 已先执行 `Queued -> Woken`，等待方必须返回成功，避免同一次
wake 既被计数又被 EINTR/ETIMEDOUT 消耗。

## Linearization

- WAIT：bucket 锁外先 fault-in 用户字，持锁执行 nofault 比较，值相等后入队；若锁内
  nofault 因瞬时缺页失败则释放锁并重试 poll，避免在映射仍有效时误返 EFAULT。
- WAKE：持 bucket 锁将至多 N 个 waiter 从 `Queued` 标记为 `Woken`。
- CMP_REQUEUE：按 bucket index 顺序同时持有两个锁，执行 nofault compare，
  然后完成 wake 与 route move。
- WAKE_OP：按相同双锁顺序执行用户字 nofault CAS，再选择两个 key 上的 waiter。

可能触发页错误的普通用户访问发生在 bucket 锁外。锁内只使用 kuaccess nofault
原子操作，因此不会在禁止抢占的 bucket 临界区里进入缺页处理。wait 比较使用
真正的只读原子 load，不会要求用户页可写或触发无意义的 COW。

唤醒调度器发生在释放 bucket 锁之后。被标记为 `Woken` 的 waiter 暂时保留在
bucket 中，由 `drain_inactive` 移除后调用其 waker。

## Bucket lifetime and lock ordering

256 个 bucket 在第一次 futex 使用时一次性创建，之后永久存在。系统不存在
动态 `key -> entry` cache，因此空 key 不需要回收，也没有 entry drop 与 wake
并发导致的 UAF 窗口。

双 bucket 操作始终先锁较小 index，再锁较大 index。waiter metadata 只能在
持 bucket 锁时修改；取消路径读取 metadata 后必须在 bucket 锁内复核。

## Robust mutex

robust owner death 只编码到用户 futex word：保留 `FUTEX_WAITERS`、清除 TID、
设置 `FUTEX_OWNER_DIED`，然后按用户字中的 WAITERS 位决定是否唤醒一个 waiter。
`kfutex` 不保存内核侧 `owner_dead` 标志，普通 FUTEX_WAIT 也不返回
`EOWNERDEAD`；该错误由 pthread mutex 协议在用户态产生。

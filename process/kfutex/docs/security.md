# kfutex — 安全与可靠性分析

## 信任边界

用户控制 futex 地址、期望值、wake 数量、bitset、requeue 目标和 wake-op
编码。`kfutex` 在这些值进入并发状态前验证地址范围与对齐；shared identity
只接受 `memspace` 解析出的稳定 backing metadata。

本 crate 没有 `unsafe` 代码。实际用户内存原子访问由 `kuaccess` 的
exception-table 边界完成。

## 核心不变量

1. Private key 必须包含 `mm_id`，不能只按虚拟地址命中其他地址空间。
2. Shared key 必须包含 object-relative page index，不能使用 VMA-relative offset。
3. bucket 永久存在；waiter 只能由 `Arc` 持有，队列中不得出现裸 waiter 指针。
4. 只有 `Queued` waiter 可被 wake 或 requeue；`Woken` 与 `Cancelled` 是终态。
5. requeue 修改 route 时必须同时持 source/target bucket 锁。
6. 锁内用户访问必须使用 nofault API；缺页处理不得持有 bucket spinlock。
7. 调度器 waker 必须在释放 bucket 锁后调用。
8. robust owner-death 状态只存在于用户字，不允许复制成内核缓存状态。

## Lock ordering

```text
lower bucket index
  -> higher bucket index
    -> waiter route or waker metadata
```

取消路径不能持 waiter route 锁去等待 bucket 锁；它读取 route 快照后释放锁，
再获取 bucket 并复核 generation，从而避免与 requeue 的反向锁序。

## 攻击与故障分析

- 哈希碰撞只增加同一 bucket 的扫描成本，不改变 key 等值检查。
- 巨大 wake/requeue count 不按 count 分配内存；操作只扫描现有 waiter。
- timeout、signal 或 future cancellation 通过状态 CAS 与 route 复核移除 waiter；
  只有 `Queued -> Cancelled` 成功才返回错误，若 WAKE 已先转为 `Woken` 则返回
  成功，避免消费一次已经计数的 wake。
- VMA 在 fault-in 与锁内 nofault 复核之间被撤销时，操作返回 `EFAULT`；WAIT
  在 enqueue 前会先在锁外 fault-in，锁内 nofault 失败时重试 poll 而不是立即
  向上返回，除非映射确实不可访问。
- CMP_REQUEUE 和 WAKE_OP 按统一双锁顺序执行，避免 ABBA 死锁。

## 已知范围

- 当前只实现 non-PI futex；PI 命令返回 `ENOSYS`。
- realtime absolute wait 在 syscall 入口一次性换算为 monotonic 相对时长；等待期间
  wall-clock 跳变不会重新调整已入队 deadline。
- timed wait 被信号打断时返回 `EINTR`；restart-block 与 `restart_syscall` 尚未实现。
- 无 timeout 的 WAIT 被信号打断时返回 `ERESTARTSYS`。

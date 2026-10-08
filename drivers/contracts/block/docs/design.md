# block — 设计文档

## 定位

`block` 是 X-Kernel 的 block core。它定义 driver-private I/O operations、`Gendisk`、
`RequestQueue/QueueLimits`、按 `dev_t` 标识的 `BlockDevice`，并拥有唯一 resident
block-device registry。

## Linux 对象映射

| Linux | X-Kernel | 所有权 |
|---|---|---|
| `struct block_device_operations` | `BlockDeviceOperations` | driver/backend algorithm、open mode callback |
| `struct queue_limits` | `QueueLimits` | logical/physical block size、alignment、minimum/optimal I/O size |
| `struct request_queue` | `RequestQueue` | canonical queue limits；request dispatch 在后续阶段加入 |
| `struct gendisk` | `Gendisk` | disk identity、queue、`part0`、state、operations |
| `struct block_device` | `BlockDevice` | `dev_t`、queue、sector range、capacity |
| `bd_holder` / exclusive `bdev_open` | `BlockDeviceClaim` | exclusive holder lifetime |
| `add_disk` / `del_gendisk` | 同名函数 | 显式 publish/unpublish |
| `blkdev_get_no_open` | `lookup_block_device` | canonical `dev_t` lookup |
| `set_capacity` | `BlockDevice::set_capacity` | 可变介质容量发布 |
| `set_disk_ro` / `get_disk_ro` | `BlockDevice::set_disk_read_only` / `is_read_only` | canonical disk state |

`BlockDeviceOperations` 不继承通用 `Device`，因为 disk identity 属于 `Gendisk`；backend
只表达 I/O 和 Linux block-device operations 层的 open/release/ioctl。`Gendisk` 持有该
algorithm object 和 `RequestQueue`，但不实现 operations trait；只有 canonical
`BlockDevice` 方法会调用 backend，从而不能绕过容量、边界和只读策略。PR5 完成 kext4
Bio 迁移前，`BlockDevice` 对旧同步 operations trait 的实现只派生 block count 并委托这些
同名方法，不保存第二份状态。disk identity、queue、state 和 operations 直接由 `Gendisk`
保存，不拆出另一个磁盘状态对象。
`BlockOpenMode` 对应 Linux `blk_mode_t`，从 KVFS 的 opened-file mode 传入 open/ioctl，
不会在 loop 等具体驱动中另建一套打开语义。

当前只创建 whole-disk `part0`。`BlockDevice` 使用 `start_sector + num_sectors` 表达 view
边界；sector 固定为 512 bytes，与 Linux `sector_t` 语义一致。后续 partition scan 可以
发布更多 view，而不引入另一种设备对象。

## 发布与查找

`Gendisk::alloc` 根据 backend 当前报告的 logical block size 建立 `QueueLimits` 和
`RequestQueue`，把 backend block count 换算成 sector capacity，并同时构造 canonical
`part0`。因此未发布 disk 已经可以用于不依赖 registry 的测试和内部装配。driver probe 随后经 block
class lifecycle 调用 `add_disk`；发布只校验 major 非零和同 major 的完整 minor range 不
重叠，再把同一个 disk/part0 identity 放入 registry，不重新创建对象。devfs、KVFS
block-special open、filesystem mount 和 boot root selection 都读取该 registry。

registry 是唯一的 `Mutex<BTreeMap<DeviceNumber, GendiskRef>>`（`BLOCK_DEVICES`）。
`add_disk`、`del_gendisk`、`lookup_block_device` 和 `block_devices` 在插入、删除、查找时
持有该锁。锁只保护映射；只读状态、claim 和容量由对象自身的同步原语保护，registry
锁内不执行 driver I/O 或 callback。

`del_gendisk` 按 part0 `dev_t` 删除 registry 持有的磁盘引用；已有 `Arc<BlockDevice>`
继续持有完整 `Gendisk`，`disk()` 始终返回同一对象，新 lookup 不再取得已撤销对象。
新介质即使复用相同 `dev_t`，也必须构造新的 `Gendisk`，从而得到新的 canonical
`BlockDevice` identity；重发同一个 `Gendisk` 仍使用它原有的 `part0`。

## 所有权与最终释放

Linux `include/linux/blkdev.h` 的 `disk_to_dev()` 使用 `part0->bd_device`，明确由 whole
block_device 对 gendisk 引用计数；`block/bdev.c:bdev_free_inode()` 在释放 whole device 时
释放 gendisk。这里按照同一生命周期表达：

```text
GendiskRef / registry ──strong──> canonical part0 (BlockDevice)
                                        │
                                      strong
                                        ▼
                                     Gendisk
                                        │
                                      weak
                                        └────────> 同一 part0
```

`GendiskRef` 只封装一个 `Arc<BlockDevice>`，通过 `Deref` 借用完整磁盘，不保存身份、容量、
队列、打开模式或 holder 状态。它替代公开的 `Arc<Gendisk>`，对应 Linux 经 `disk_to_dev()`
获取/释放磁盘引用。该拥有引用是必要的：若直接公开内部 `Arc<Gendisk>`，调用方可以单独
保留磁盘而释放 part0，破坏 canonical whole-device 生命周期。

`Gendisk::alloc` 返回 `GendiskRef`，通过 `Arc::new_cyclic` 一次建立全部关联后才返回。
内部 `Arc<Gendisk>` 不对外暴露；`BlockDevice::disk()` 仅返回借用。因此每次对磁盘的借用
都由 live part0 保活，`Gendisk::part0()` 的 weak upgrade 必须成功，不是可恢复的设备缺失。
最后一个磁盘拥有引用、设备引用或 claim 释放后，part0 释放磁盘；磁盘的弱回指不形成环。
整个构造、借用和回收过程不需要 unsafe。

当前仅构造 whole device。未来加入分区时，分区的所有权也必须保活 canonical part0，
不能直接暴露内部 disk Arc 或独立构造可逃逸的 `Gendisk`。

## 独占持有

`BlockDevice::claim_exclusive()` 返回 RAII `BlockDeviceClaim`，对应 Linux block holder
所有权。一个 canonical device 同时只允许一个 holder；filesystem superblock 直接持有该
token，并在初始化失败或 final shutdown 进入 dead 后释放。因此不同 filesystem instance
不能同时拥有同一介质，且 block core 不需要了解 VFS 或文件系统类型。

## I/O 边界

`BlockCompletionOperations` 表达设备完成回收能力：一次调用必须有限、不睡眠、不提交新
请求。host 保证同一注册的回调串行执行；activation 保持目标强引用直到同步停止。
一个目标只注册一次。排队和停止的具体所有权由 host 管理，不暴露内核 API 给驱动。

`completion` 模块定义 OS-neutral 的 `PrepareBlockWait`、`BlockWaiter` 和
`BlockSignals`。它们不改变 `BlockDeviceOperations`，也不直接引入内核调度依赖。
host 注入普通准备函数，返回任务绑定的 waiter，由 waiter.signals() 导出同一通知状态的 Arc，
驱动通过契约等待准入提示及最终完成。准入提示不授予提交权；队列/FIFO 谓词和
请求结果仍归具体事务实现管理。终态通知必须在设备释放数据缓冲区后发布。

waiter 不跨任务共享；signals 可以在 IRQ 中调用且不保留 request/data/device 指针。
等待资源准备和准入注册允许返回错误，但已提交后的终态等待不再注册或返回等待
错误。VirtIO 实现在 `virtio::blk`，等待及 IRQ/softirq 接入由 `kdriver` 提供；
其他驱动接入这些契约不需要修改文件系统。

`QueueLimits` 是设备几何的唯一 block-core 来源。当前 backend 只报告 logical block size，
所以 physical block size 和 minimum I/O size 按 Linux 默认值取 logical block size，optimal
I/O size 与 alignment offset 为零。logical block size 必须至少 512 bytes、为 2 的幂且可由
`u32` 表示，并受当前 4-KiB page 对应的 Linux `BLK_MAX_BLOCK_SIZE` 上限约束。

`BlockDevice` 在委托 backend 前校验 buffer 是 logical-block-size 整数倍，把 legacy backend
block ID 换算为 sector extent，检查完整 extent 不越过 `num_sectors`，再把加入
`start_sector` 后的 sector 转回 backend block ID。全部乘法和加法使用 checked arithmetic。
`Gendisk::alloc` 把初始 backend block count 换算为 sector；`set_capacity` 与 Linux 一样接收
sector count。两条路径都要求 `num_sectors * 512` 可由 `u64` 表示。
容量消费者直接读取 `num_sectors` 或 `size()`；legacy backend block count 只在构造边界换算
一次，不作为第二套设备容量状态。

backend 通过 `BlockDeviceOperations::is_inherently_read_only()` 报告设备生命周期内不可变的
固有只读能力，`Gendisk::alloc` 在发布前保存该能力。`BLKROSET` 控制的管理只读状态单独保存在
`Gendisk` 的原子状态中；有效只读状态是“固有只读或管理只读”。因此 `BLKROSET 0` 只能清除
管理状态，不能把固有只读介质变为可写。`BlockDevice::write_block` 在进入 backend 前统一拒绝
有效只读设备的写入。

KVFS 负责 Linux `blkdev_read_iter` / `blkdev_write_iter` 对应的字节适配：完整对齐块直接
传递调用方 buffer，首尾 partial block 复用单个 read-modify-write scratch buffer。普通
write 不等价于 durability barrier；只有显式 `fsync` 才调用 backend `flush`。

## Non-Responsibilities

- No device I/O execution: reading, writing, and flushing media belong
  to the concrete backend behind `BlockDeviceOperations` (for example
  `virtio::blk`); the core only validates and routes.
- No IRQ wiring or completion threading: waiting, admission, and
  IRQ/softirq delivery are owned by `kdriver`'s block host providers;
  this crate defines the contracts only.
- No filesystem or page-cache policy: byte-level adaptation, durability
  semantics, and mount ownership live in KVFS and the filesystem layer.
- No partition scanning: only whole-disk `part0` is published today.
- No media encryption or access control beyond the read-only and
  exclusive-claim rules documented here.

## IRQ Completion Requirements

These frozen requirements describe caller-visible behavior, architecture
constraints (08-09), and delivery obligations (11), not a particular completion
mechanism. Sources are the user's blk_irq scope decisions, the block API at
baseline 80574592, and the repository driver-boundary rules. PR 727 supplied
problem context only; its implementation is not a design or code input.

Scope: ordinary reads, writes and flushes through filesystem/block interfaces;
virtio-blk is the first converted driver. The synchronous API remains unchanged.
Automatic polling fallback, conversion of every driver, forced DMA cancellation,
and unrelated scheduler/preemption repairs are not included. IRQ setup failure
fails activation without publishing a disk (accepted integration policy).

An accepted request has passed block validation and been accepted for device
processing. Continuous polling means repeatedly checking device completion for
the duration of outstanding I/O instead of waiting for a completion event.

### REQ-BLKIRQ-01: Preserve caller-visible block operation semantics.

- Source: current block API and user clarification.
- Behavior: ordinary callers continue to observe synchronous read, write, and
  flush operations. Success means the requested data transfer or flush effect
  required by the existing block contract has completed before the call returns.
- Acceptance: existing filesystem and block-device call sites compile without
  API changes; existing block read/write/flush behavior tests remain valid.

### REQ-BLKIRQ-02: Keep ordinary filesystem block I/O on the general block path.

- Source: user clarification and current X-Kernel call paths.
- Behavior: ordinary filesystem block reads, writes, and flushes reach concrete
  block drivers only through the normal block interfaces used by the rest of the
  system. Filesystems do not need to know that virtio-blk is the backend in
  order to benefit from interrupt-driven completion.
- Acceptance: code review shows filesystem and block-device-file paths do not
  call virtio-blk-specific APIs for ordinary block I/O.

### REQ-BLKIRQ-03: Complete virtio-blk requests without continuous polling.

- Source: user clarification and current virtio-blk polling behavior.
- Behavior: for an accepted virtio-blk read, write, or flush on an IRQ-capable
  device, the waiting caller does not continuously poll device completion state
  until the request finishes.
- Acceptance: test, trace, or instrumentation demonstrates an accepted
  virtio-blk request whose completion is observed after device interrupt
  notification and whose waiting caller is not on the continuous polling path.

### REQ-BLKIRQ-04: Return exactly one result for each accepted request.

- Source: synchronous block API expectation and PR review discussion about
  completion races.
- Behavior: each accepted read, write, or flush returns one completion result to
  its original caller. The result is not duplicated or lost when multiple
  requests or multiple block devices complete close together.
- Acceptance: tests or review evidence cover multiple completions and show every
  accepted request returns exactly once.

### REQ-BLKIRQ-05: Do not lose completion across wait races.

- Source: PR review discussion and requirement for accepted requests to finish
  visibly.
- Behavior: if a request completes before, during, or after the caller begins
  waiting for it, the caller can still observe the completion result.
- Acceptance: tests or reviewed interleavings cover completion-before-wait,
  wait-before-completion, and concurrent completion/wait setup.

### REQ-BLKIRQ-06: Preserve validation and read-only outcomes.

- Source: current block contract and user requirement that FS I/O remains on the
  general block path.
- Behavior: invalid block size or extent requests are rejected according to
  existing block behavior. Writes rejected because the effective block device is
  read-only are observed as `DriverError::ReadOnly`.
- Acceptance: existing block validation and read-only tests remain valid; tests
  or review evidence show the IRQ-capable virtio-blk path cannot change these
  caller-visible outcomes.

### REQ-BLKIRQ-07: Preserve caller-visible error meaning.

- Source: current `DriverError` contract and PR review discussion.
- Behavior: ordinary device I/O failures are reported as I/O failures.
  Temporary inability to accept more device work is reported as a retryable or
  backpressure condition, not as internal state corruption.
- Acceptance: tests or review evidence cover ordinary device failure and
  queue/backpressure failure mapping.

### REQ-BLKIRQ-08: Keep the new completion behavior reusable for future block drivers.

- Source: user clarification and Linux reference.
- Behavior: adding interrupt-driven completion for virtio-blk does not require
  filesystem-facing changes that would have to be repeated for another
  IRQ-capable block driver.
- Acceptance: design review can trace how another concrete block driver would
  expose the same caller-visible completion behavior without filesystem changes.

### REQ-BLKIRQ-09: Preserve reusable-driver portability boundaries.

- Source: project driver boundary rule.
- Behavior: reusable concrete driver crates remain independent from X-Kernel
  host-kernel implementation crates for host-specific capabilities.
- Acceptance: dependency metadata and imports show no new direct dependency from
  `drivers/devices/virtio` to host crates such as `kirq`, `ktask`, `kwork`,
  `khal`, `memspace`, `kdma`, or `kruntime`.

### REQ-BLKIRQ-10: Demonstrate reduced CPU waste under delayed I/O.

- Source: user focus on replacing polling with IRQ handling and PR performance
  motivation.
- Behavior: under an I/O-latency scenario, a task waiting for virtio-blk I/O
  should not consume CPU as if it were continuously polling for the full wait.
- Acceptance: runtime evidence, benchmark evidence, or instrumentation compares
  the new path with current polling under a delayed-I/O workload. If the
  environment cannot provide stable evidence, the missing validation and reason
  are recorded.

### REQ-BLKIRQ-11: Keep records, docs, and tests aligned with accepted behavior.

- Source: project change-completeness rules and loaded collaboration skill.
- Behavior: task records, affected design/security docs, rustdoc for touched
  public APIs, and regression tests reflect the final accepted behavior.
- Acceptance: documents are updated where behavior or contracts changed; focused
  tests are added or updated; build/lint/test commands are run according to the
  project workflow or recorded as not run with reasons.

## Completion Integration

The synchronous caller owns its buffers until return. Concrete drivers own
hardware submission, request identity and buffer retirement; host providers own
waiting and deferred execution. `block` defines the shared contracts without
owning hardware tokens or scheduler objects. Filesystem callers use
`BlockDevice`/`Gendisk`, never a VirtIO-specific wait or interrupt API.

- [VirtIO transactions and resource lifetime](../../../devices/virtio/docs/design.md)
- [Host waiting, IRQ dispatch and removal](../../../integration/kdriver/docs/design.md)


## 分阶段迁移与测试边界

PR1 只收敛磁盘生命周期、sector 容量和队列几何，不建立第二条请求提交路径。
main 已有的 virtio 请求等待、IRQ 完成回收和关闭排空契约继续生效；后续 Bio/Request
迁移应复用并收敛这些完成机制，而不是并行保留两套请求所有权和完成状态。

未发布磁盘可构造表示测试不必注册全局设备，不等于整个文件系统 crate 已经能够在
宿主进程独立运行。当前 `block -> ksync -> ktask` 以及
`kext4 -> kvfs -> ksignal/memspace` 等依赖仍会引入内核运行环境。直接执行
`cargo test -p kext4` 的分配器配置、链接和运行时依赖问题，需要单独明确测试层次：
纯磁盘格式/算法测试可以在宿主执行，涉及真实块层、VFS、调度和页缓存的集成测试
通过配置后的内核单测路径执行。若要让同一套存储算法支持宿主测试，应在已有的
能力与集成边界上隔离运行环境；不能为测试新增平行设备模型、伪造内核链接符号，
或只跳过无法链接的测试来宣称宿主测试通过。

# kalloc-types 设计

## 范围和接口

首批只提供 `KBox<T>`：单对象独占所有权、可失败构造、`Deref`、
`DerefMut` 和自动释放。暂不提供 DST、raw pointer 转换、克隆、容器或
pinning。调用方不接触标准 `Box`，后续可以在内部演进分配机制。

构造接口统一返回 `KResult<KBox<T>>`，不使用 `try_` 前缀：

| 接口 | 策略 |
| --- | --- |
| `KBox::new(value)` | 默认 `AllocFlags::KERNEL`，调用方允许睡眠、回收 IO 和文件系统回调 |
| `KBox::new_nowait(value)` | `AllocFlags::NOWAIT`，不允许分配路径睡眠 |
| `KBox::new_with_flags(value, flags)` | 调用方选择常用组合或组合许可位 |

分配不足返回 `KError::NoMemory`；未知 flag 位返回 `KError::InvalidInput`，
包括零大小值。失败均析构输入值。值构造过程中的嵌套分配和输入值自身
析构的阻塞行为不属于这个 API 的分配策略保证。

## 分配上下文

`AllocFlags` 借鉴 Linux GFP 的“许可位 + 常用组合”，不是 Linux 位值 ABI。
`DIRECT_RECLAIM` 允许可睡眠的同步回收，`IO` 允许回收发起 IO，`FS` 允许
回收进入文件系统。没有 `DIRECT_RECLAIM` 时，即使 IO/FS 位存在也不得
通过它们执行睡眠回收；有此位时调用方必须允许睡眠。

`KERNEL` 包含全部三项许可，也是 `Default`；`NOWAIT` 不授予任何许可；
`NOIO` 只允许同步回收，`NOFS` 允许同步回收和 IO。可通过位运算构造策略，
例如 `AllocFlags::KERNEL & !AllocFlags::FS`。这些是许可而不是必须执行的
行为，因此当前无回收后端仍能满足限制。未加入储备池、必成功、DMA 或
任意 allocator 注入；这些功能不能靠设置许可位假装实现。

默认构造只能在允许睡眠的任务上下文调用，不能持非睡眠锁或在中断里用。
这一约束现在就成立，不能依赖当前后端恰好不睡眠。`new_nowait` 不承诺
储备访问或有界延迟。策略仅作用于单次分配，不会改变 task-wide 上下文。
Linux 推荐通过作用域 API 处理跨调用链的 NOFS/NOIO 约束；本次不实现这类
作用域继承，因此调用方不能假设嵌套分配自动继承限制。

当前所有已知许可组合都调用 `kalloc::global_allocator().alloc`，现有后端
不执行睡眠回收、回收 IO 或文件系统回调。未来必须在内部 `allocate` 将
许可传递给回收后端，保留 NOWAIT、NOIO 和 NOFS 的限制。
不能改走当前 `GlobalAlloc` 入口，该入口会在 OOM 时调用 fatal handler。
后端本身的内部不变量 panic 不转换成普通分配失败。
直接分配仍参与 `kalloc` 的 RustHeap 用量统计，但不经过 `GlobalAlloc`
包装层的可选 backtrace tracking；未来统一跟踪入口时需要保留失败语义。

## 生命周期

非零大小值使用 `Layout::new::<T>()` 申请，成功后一次写入完成初始化。
ZST 使用对齐的 dangling 指针，不调用分配和释放接口，但正常运行析构。
`KBox` 析构值，私有 `Storage` 随后释放内存；即使值析构展开，字段清理
仍释放存储。当前内核通常采用 panic abort，展开行为不是内核恢复保证。
所有权移动不改变堆地址，但 `DerefMut` 允许替换值，因此不提供 pinning。
`Send` 和 `Sync` 分别要求 `T: Send` 和 `T: Sync`。

## 调用点迁移约束

不能批量把 `Box::new` 改成默认 `KBox::new`。每个迁移点都必须沿调用链
核对以下条件，并在所属接口的 rustdoc 中说明执行上下文和错误行为：

1. 确认是可睡眠任务上下文，还是中断、关 IRQ/抢占、非睡眠 RCU 读侧或
   持自旋锁的路径。默认 `new` 只用于前者，ZST 和当前不回收都不是例外。
2. 检查当前持有的全部锁与潜在回收路径的依赖。可睡眠 mutex 并不自动
   意味着能调用默认分配；回收可能回到相同子系统并等待该锁。按实际
   依赖选择 NOFS/NOIO、调整分配位置或使用 NOWAIT。
3. NOFS/NOIO 仍允许睡眠；不允许睡眠时必须去掉 DIRECT_RECLAIM。
   NOWAIT 只约束存储分配，不承诺 NMI 安全、任意锁上下文的可重入性、
   储备或必成功。仍须核对底层 allocator 的锁顺序和重入条件。
4. 核对输入值构造、失败时的析构，以及成功后最终释放所在上下文。
   flags 不会约束这些代码，也不会被保留下来影响未来 Drop；Deref 本身
   不分配或睡眠，但通过引用调用的 T 的方法有自己的约束。
5. 确认错误能返回或有明确回退，失败前未发布半成品，资源与已有状态能
   正确清理。不能通过新增 unwrap/expect 把可失败申请改回致命失败。

不根据当前是否持锁隐式切换默认策略：策略是调用接口的稳定契约。
同一辅助函数若需要服务多种上下文，应让上层传入 flags，或明确拆分
普通/非睡眠入口，不能在下层无条件使用默认 `new`。

## 首个调用方和验证

VFS inode 的文件锁上下文采用 `Once<KBox<FileLockContext>>`。
`try_call_once` 在失败后保持未初始化状态，下一次调用可重试；成功后
所有请求复用同一上下文。由于 Once 的竞争者自旋，初始化使用 `new_nowait`。
`lock`/`flock` 将失败作为 `NoMemory` 返回，查询和无上下文的解锁无需分配。
上下文里的锁记录节点仍使用现有实现，不属于本次迁移。

guest 测试覆盖实际分配、对齐、移动、可变访问、析构、ZST 和局部失败注入。
失败注入仅向私有分配回调返回错误，不引入全局故障开关；VFS 测试验证
失败不发布上下文、重试成功和已初始化时不再次执行分配。
策略测试覆盖默认构造、常用及自定义 flags 到私有分配回调的传递，以及
未知位拒绝时不执行分配且正确析构输入。

定向执行命令为 `make unittest UNITTEST_CRATE=kalloc_types,kvfs`；
过滤器使用 Rust crate 名称 `kalloc_types`，而不是 Cargo 包名中的连字符。

## 参考

- [Linux 内存分配指南](https://docs.kernel.org/core-api/memory-allocation.html)：GFP 许可、常用组合及 NOFS/NOIO 作用域建议。
- [Linux Rust Box](https://rust.docs.kernel.org/kernel/alloc/kbox/struct.Box.html)：`new(value, flags)` 是可失败构造；本模块另外提供无 flags 的默认入口和 NOWAIT 快捷入口。

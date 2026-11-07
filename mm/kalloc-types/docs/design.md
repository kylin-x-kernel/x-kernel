# kalloc-types 设计

## Minimal Shared Ownership

`KArc<T>` and `KWeak<T>` adapt the sized shared-owner surface from PR #385,
head `e3042051e558cf65d8533dfb2edab2a2e3152648`. This migration deliberately
retains sharing, downgrade/upgrade, identity/count queries and try_unwrap,
but excludes cyclic construction, DST coercions, mutable
uniqueness and whole-kernel Arc replacement. The original manual reference
counter is not copied: its Sync bounds omitted Send, its final-value Drop did
not guard the implicit weak release against unwinding, and overflow used a
post-increment panic rather than preventing a wrapping count.

The private representation is standard `Arc<T, SharedAllocator>` and its Weak
counterpart, using the pinned Rust toolchain's allocator_api implementation.
Rust supplies strong/weak synchronization, overflow protection and cleanup
when the payload unwinds. Auto traits inherit the Send + Sync payload
requirements; no unsafe Send/Sync implementation is added by this wrapper.
The allocator's permission field is carried by handles but is not a release
token: raw recovery uses equivalent NOWAIT allocator metadata. Unlike PR #385's
single-pointer representation, this wrapper must not be assumed pointer-sized
or layout-compatible with standard Arc<T>. No representation/ABI guarantee is
exported; broad call-site migration needs a separate footprint review.

`new`, `new_nowait` and `new_with_flags` return KResult and use the same
layout allocator as KBox. Unlike `Arc::try_new` with Global, the custom
allocator calls kalloc's fallible inherent allocation method rather than the
fatal GlobalAlloc adapter. Unknown flags fail before allocation, even for ZST
payloads. Every shared owner needs a count header, so ZST construction can
return NoMemory. Hosted builds retain paired nullable host allocation/free.

The allocator is private and copied with immutable flags; Arc owns its exact
layout and transfers the live allocation once at final weak release. Cloning,
downgrading, upgrading and empty weak construction do not allocate. Dropping
the last strong owner destroys T in the caller's context; weak owners retain
only storage. Final weak drop deallocates without reclaim. Neither operation
inherits the constructor's sleeping permission, and neither claims NMI safety
or bounded latency. Callers must audit T::drop and allocator lock order.

Guest tests cover ownership/counts, weak lifetime, extraction, alignment, ZST
allocation, invalid flags and local allocation-failure injection. Hosted tests
also cover concurrent upgrade/final release and payload unwinding. Hosted
compile-fail doctests reject Sync wrappers around a Sync-but-not-Send payload.
These tests do not simulate global OOM or prove timer admission/progress; timer
and signal protocol work remains separate.

### Shared Raw Ownership

`KArc::as_ptr` borrows an address without adding ownership. `into_raw`
transfers exactly one strong reference, and unsafe `from_raw` recovers that
reference without changing its count, allocating or sleeping. Multiple
transfers may have identical addresses but each retains a separate reference.
Pointer copies alone do not create ownership. The reference must be recovered
once, with the same type and provenance; it cannot be mixed with standard Arc,
Weak or KBox pointers. The implementation delegates pointer extraction to Arc,
not a pointer derived through Deref, preserving raw allocation provenance.

SharedAllocator flags and its test failure switch only affect allocation.
All instances share the same build-time kernel/host backend and layout-based
deallocator, so raw recovery can reconstruct an equivalent release allocator.
No caller-visible policy or allocator state is recoverable from the pointer.
Introducing arenas, per-allocation release tokens or stateful backends requires
revisiting this representation before retaining the pointer-only interface.

The optional `intrusive-collections` feature exports `KArcPointerOps<T>` for
concrete adapters using the existing library. This zero-sized implementation
transfers exactly one KArc strong reference per occupied link, with no allocation
or custom queue protocol. The default configuration does not enable this optional
dependency.
Hosted tests use this production pointer adapter with test-only link projections.
They cover duplicate-insertion panic recovery, known-member removal, independent
linkage fields, normal list teardown and the library's own take operation for
moving owners without destruction. Link selection remains subsystem-specific; these
tests do not establish a production timer queue's locking or cancellation
protocol. Production integration must encapsulate those obligations separately.
See `tests/README.md` for default and opt-in host/Miri commands.

## Raw ownership and hosted validation

`KBox::into_raw` transfers the sole owner without touching its pointee;
`unsafe KBox::from_raw` recovers that owner with the original type, provenance
and DST metadata. `KBox::leak` instead consumes the owner and yields an
unbounded borrow, retaining both value and storage without running `T::drop`;
it is reserved for deliberately permanent data such as `'static` boot state or
test fixtures. None of these operations allocate or sleep. Only pointers
originating from KBox are accepted; this is not interoperability with standard
Box or foreign allocators. RCU integrations retain raw ownership until old
readers finish.

Bare-metal builds continue using the fallible kalloc backend directly.
Hosted builds use paired nullable host allocation and deallocation for host/Miri
ownership tests. They do not model reclaim, IRQ context or kernel OOM behavior.


## Migration Batch Boundary

Owner migration must preserve both failure cleanup and subsystem progress.
An allocation that was previously infallible cannot be replaced by a fallible
call until callers have an error, rollback or preallocated-resource protocol.
NOWAIT does not guarantee success; KERNEL grants sleeping reclaim permissions
but does not implement a progress guarantee. Adding expect, discarding accepted
work, or retrying indefinitely is not a substitute for that protocol.

The current runtime/callback migration batch can close with the following
explicitly deferred production paths. They remain standard Box owners, not
completed migrations or permanent exemptions:

| Deferred path | Required separate design and validation |
| --- | --- |
| `ktask::TaskEntry::Closure` and the legacy kernel task factory | Audit all spawn callers, boot/IRQ-disabled contexts and publication boundaries. For dynamic workqueue workers, define Creating-slot rollback, baseline startup failure, retry wakeups and continued execution under memory pressure. Review whether reclaim-facing services need preallocated execution resources. Validate sustained allocation failure, pending demand, concurrent stop and partial startup. |
| `ksignal::PendingSignals::info_std` and `put_signal` | Define allocation failure separately from standard-signal coalescing. Keep pending bits and payload publication consistent under the signal lock. Preserve fatal-signal behavior and POSIX timer dequeue identity/acknowledgement; do not silently discard metadata. Review preallocation or bounded storage and validate exhaustion, coalescing, dequeue and timer lifetime races. |

These paths require separate design review, implementation and validation rather
than expanding the ownership-replacement patch into an unreviewed subsystem
redesign. Prepared user entries and workqueue manager entries already have
explicit failure boundaries; they do not make legacy kernel spawning, dynamic
worker growth or signal delivery fallible end to end. Other subsystems and
test-only Box uses remain subsequent migration work. The system-Box lint stays
deferred until the remaining migration and any reviewed exceptions are resolved.

## Moving A Sized Value Out

`KBox::into_inner(owner)` consumes a sized owner and returns its value without
cloning or running the value's destructor. A ManuallyDrop owner prevents the
ordinary KBox destructor from touching the moved value. The existing Storage
guard releases the original allocation with `Layout::new::<T>()` after the
pointer read; aligned zero-sized values require no storage release.

Extraction does not allocate, sleep or enter reclaim, but still requires the
allocator's deallocation lock order and reentrancy constraints. It does not
transfer allocation flags to the returned value's use or destruction. This
operation neither supports unsized extraction nor changes the existing callback
forwarding bound: a sized consuming-only FnOnce can be extracted and invoked,
but `KBox<dyn FnOnce()>` is not callable through this API. This supplies the
move-out primitive needed for pending payloads and consuming entry ownership;
it does not resolve their allocation failure or publication contracts.

Guest tests cover non-Copy destructor ownership, mutated aligned values with
owned arrays, aligned ZSTs, and a closure that consumes its captured resource.

## Fallible work callbacks

`KBox` forwards reusable `Fn` and `FnMut` callbacks, including erased trait
objects and borrowed captures, using `fn_traits`, `unboxed_closures`, and
`tuple_trait`. Arguments and return values are forwarded without allocation or
new unsafe code. The required `FnOnce` implementation invokes `FnMut` through a
mutable borrow and then drops the owner; consuming-only `FnOnce` closures are
not supported. Allocation flags do not constrain callback execution or Drop.

## Fallible directory iterators

KBox<T: Iterator + ?Sized> delegates next, size_hint and nth to T. This supports
owned chain/enumerate/skip adapters even after erasure to a borrowed-lifetime
trait object. Forwarding adds no allocation and does not introduce unsafe code.

## 范围和接口

The initial API provides `KBox` for exclusive ownership of a single object,
fallible construction, `Deref`, `DerefMut`, and automatic destruction.
`KBox<T: ?Sized>` allows sized owners to unsize into trait objects or slices
without allocating, preserving pointer metadata and borrowed lifetimes.
Runtime-length slices support uninitialized reservation and Copy-value filling.
`from_slice` and `from_slice_with_flags` copy an existing kernel slice into
fallible ownership, without intermediate Vec allocation or user-memory access.
The default uses KERNEL; explicit flags retain the existing allocation context
contract. Copy ZST slices require no per-element work.
String construction, cloning, growable containers, and
pinning are not provided.
Callers do not use the standard `Box` through this API.

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
`KBox` stores only `NonNull<T>` and an ownership marker, with a transparent
representation and a derived `CoercePointee` implementation. Following Linux
KBox's DST ownership model, it does not store a layout in every owner.
Before destroying the value, it computes `Layout::for_value` from the still-valid
value and creates a private stack-local `Storage` guard. The guard frees storage
with the saved layout and never creates a reference after value destruction.
The guard also frees storage if destruction unwinds. The kernel normally uses
panic abort, so this is not a kernel recovery guarantee. Unsizing neither
allocates nor changes the allocation-policy contract.
所有权移动不改变堆地址，但 `DerefMut` 允许替换值，因此不提供 pinning。
`Send` and `Sync` require `T: Send` and `T: Sync`, respectively, including for DSTs.

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

## Second batch: mount-transaction private state

Both private-state slots in `FsContext` use `KBox<dyn Any + Send + Sync>`.
After parsing initialization options, tmpfs/ext4 allocate owners with the default
KERNEL policy before mount publication or superblock reconfiguration locks.
The interfaces require sleepable task context with no locks needed by reclaim.
The process fs_context snapshot lock is released before transaction construction.
`set_private` and `set_super_private` return `VfsResult<()>` and allocate before
replacing the slot, preserving the old state on failure. Callers propagate errors
without introducing a fatal OOM path for owner allocation. Persistent superblock
private state and other Box uses remain for subsequent migration batches.

New tests cover borrowed trait objects, slices and empty slices, dynamic
destruction of aligned ZSTs, owner layout, and positive Send/Sync bounds.
VFS tests cover private-state replacement and cleanup after initializer failure.
Existing local allocation-failure injection still verifies that the input value
is destroyed exactly once.

## 参考

- [Linux 内存分配指南](https://docs.kernel.org/core-api/memory-allocation.html)：GFP 许可、常用组合及 NOFS/NOIO 作用域建议。
- [Linux Rust Box](https://rust.docs.kernel.org/kernel/alloc/kbox/struct.Box.html)：`new(value, flags)` 是可失败构造；本模块另外提供无 flags 的默认入口和 NOWAIT 快捷入口。

## Reserved owner storage

`AsRef<T>` and `AsMut<T>` expose the same lifetime-bounded pointee borrows as
Deref/DerefMut, including for DSTs. Debug delegates directly to T's formatter,
without adding allocation; the formatter retains its own context requirements.

`KBox::new_uninit` reserves a sized `MaybeUninit<T>` owner with KERNEL
permissions before a caller performs an irreversible operation.
`new_uninit_with_flags` provides explicit permissions, including NOWAIT;
reservation does not change the context requirements of later initialization.
`KBox::write` consumes the reservation, writes a complete value and returns
`KBox<T>` at the same address without allocating or failing. Abandoning a
reservation only frees storage, without running a destructor for an unwritten T.
This is not field-by-field initialization, pinning, or a runtime-sized allocator.

## Runtime-length slice storage

KBox<[T]> reserves Layout::array::<T>(len) directly, without a temporary Vec.
Unknown flags and overflowing layouts return InvalidInput before allocation;
backend failure returns NoMemory. Empty and ZST slices retain length and aligned
non-null pointers without allocating. new_uninit_slice uses KERNEL; its explicit
flags variant has the same context requirements as sized allocation.
new_slice fills Copy values without intermediate ownership or a fallible loop.
For Copy ZSTs it skips the fill loop entirely; the supplied valid value proves
inhabitation and there are no bytes to write or element destructors to run.
Unsafe assume_init transfers the original length and pointer and requires every
element to be valid. No generic fallible element initializer is exposed.

### Reference implementations and deliberate differences

This comparison uses the local Linux v7.2 tree at
`8d3ae59288f1e7d58d76558a6ee96d533bc5019f` and Rust 1.95.0 library sources
from rustc commit `59807616e1fa2540724bfbac14d7976d7e4a3860`.

- Rust `library/alloc/src/boxed.rs`, `try_new_uninit_slice_in`, uses an aligned
  dangling pointer for empty/ZST slices and checks array layout for nonzero
  storage. Its slice `assume_init` preserves the pointer and length. Our API
  follows those ownership rules, but validates allocation flags even when no
  allocation is needed and reports layout/flag errors as InvalidInput.
- Linux `rust/kernel/alloc/kvec.rs`, `Vec::capacity`, uses usize::MAX for ZST
  capacity. Neither this nor Rust's ZST reservation imposes an arbitrary element
  limit. Byte-layout validity is not a work-budget guarantee.
- Rust `library/alloc/src/vec/spec_from_elem.rs` specializes repeated unit values
  by setting the length without cloning elements. Generic repetition must
  preserve observable Clone behavior. Our new_slice explicitly copies T; it
  never calls Clone, so skipping writes for every inhabited Copy ZST is valid.
  This optimization must not be extended to a future Clone-based initializer.
- Linux `rust/kernel/alloc/kbox.rs`, `Box::pin_slice`, initializes elements one
  at a time and records the initialized prefix in a Vec for failure cleanup.
  A future fallible initializer here would need equivalent prefix ownership;
  the current unsafe assume_init API is not a substitute for that guard.
- Linux Box drop computes the dynamic layout and explicitly destroys its value
  before freeing storage. Our Storage guard additionally preserves freeing on
  unwind. Rust's standard Box has compiler-assisted pointee destruction before
  its handwritten Drop body; copying that body would omit destruction here.

# kalloc-types 安全与可靠性

## Shared Owner Boundary

KArc and KWeak expose only sized, typed ownership. They do not export the
internal allocator or permit reconstructing owners from foreign pointers.
The standard Arc implementation owns the strong/weak counters and their
ordering; its Send/Sync constraints require both Send and Sync from T.
This avoids PR #385's weaker Sync bound, which could permit cloning/upgrading
on a different thread and transferring destruction of a non-Send payload.

The unsafe implementation includes SharedAllocator and raw KArc recovery.
Allocation returns an
aligned region for the requested Layout through KBox's backend; deallocation
accepts only the matching live allocation transferred by Arc after all strong
and weak users finish. Zero-size allocator requests return an aligned dangling
region and require no free; actual Arc count headers are nonzero-sized even
when T is a ZST. The host allocator pair and kernel allocator pair never mix.

| Failure / risk | Handling and remaining obligation |
| --- | --- |
| Invalid flags or reservation OOM | Return InvalidInput or NoMemory; input drops once; no owner published |
| Upgrade racing final strong drop | Standard Arc either grants a live strong reference or returns None; no resurrection |
| Payload destructor unwinds | Standard Arc releases the implicit weak through its guard; surviving weak handles cannot upgrade |
| Final weak release in an unsuitable context | Caller must obey allocator lock/reentrancy constraints; constructor flags are not destructor permissions |
| Strong reference cycle | Leak unless callers break the cycle; KWeak is available for back-references |
| Reference-count overflow | Inherited standard Arc abort protection, not a recoverable allocation failure or wrapping counter |
| Raw reference recovered twice, from an address borrow, or from another owner type | Violates from_raw's unsafe contract; one unclaimed KArc strong reference is required |
| Stateful allocator introduced without preserving its release authority | Invalidates pointer-only raw recovery; backend changes must revise the representation and recovery proof |

`into_raw` does not decrement the count or drop the payload. Recovery uses the
same SharedAllocator type and allocation domain, with canonical NOWAIT flags;
release never inspects allocation flags or the test-only failure switch. This
is equivalent release authority, not conversion to Global or reconstruction
from an arbitrary address. as_ptr does not extend lifetime or grant mutable
access. Transferring a raw pointer does not bypass Send/Sync requirements.
Final strong/weak destruction after recovery retains the normal context rules.

Raw round-trip tests cover both allocation policies, independently transferred
references, extraction, aligned ZSTs and weak release through recovered
allocator metadata. Optional KArcPointerOps delegates directly to that raw-owner
contract, without a new allocator, refcount or queue identity. Its safety depends
on the concrete adapter preserving link projections and the library returning
each transferred pointer once. Conversion does not authorize arbitrary removal
or release under a lock; those obligations remain with the subsystem.
Library duplicate-insertion panics and fast_clear's intentional ownership leak
are not converted into recoverable cancellation. Normal teardown is covered;
complete draining after a panicking payload destructor is not guaranteed by
this adapter. Kernel panic handling and any hosted unwind-cleanup protocol remain
outside its ownership-conversion contract.
Hosted intrusive_collections probes exercise independent links, duplicate-insertion
panic recovery, removal and normal list teardown. Production adapters must own
and release every linked reference and enforce the list's removal preconditions.

Allocation-failure injection changes only the private allocator result before
the same Arc::try_new_in call used in production. It does not inject payload
construction failure or global allocator exhaustion. Concurrent and unwind
tests are hosted; guest tests do not unwind the kernel. No timer call site is
migrated by introducing these types, and no complete OOM-safety claim is made.
The empty-weak test checks identity/upgrade behavior, not allocator traffic;
invalid-flag tests check the returned error and input cleanup. Allocation-free
clone/downgrade/upgrade and validation-before-allocation are additionally
established by source inspection, not an allocation-counting regression test.

## KBox raw ownership boundary

A raw transfer suppresses owner destruction, not pointee initialization.
Recovery requires exactly one outstanding ownership right and the original
non-null, aligned pointer, allocation provenance, type and DST metadata.
No conflicting references may remain. Repeated recovery, foreign pointers and
premature recovery during RCU reads violate the unsafe contract.
`KBox::leak` is the safe alternative that never recovers ownership: the caller
receives an unbounded borrow, so the allocation and value are intentionally
retained for the remainder of execution.
Dynamic drop still computes the layout while the value is valid, then destroys
it once and frees with the matching backend. ZSTs never allocate or deallocate.
Tests cover sized/DST address and metadata retention, single destruction,
aligned ZSTs and empty slices; no malformed pointer is passed to the unsafe API.

Hosted allocation returns NoMemory on null and frees through the same host
allocator. Kernel allocation never takes that path. Host/Miri results establish
ownership properties, not kernel allocator timing or execution-context safety.


## Copied Slice Ownership

`from_slice_with_flags` reserves the complete slice before copying valid Copy
elements. The source remains unchanged on error, and assume_init runs only after
all non-ZST elements have been written. ZST validity comes from the source slice;
Copy excludes destructors and no per-element loop is needed. Tests cover aligned
elements, independent ownership, empty slices, invalid flags and huge ZST slices.

## Sized Ownership Extraction

`KBox::into_inner` takes exclusive ownership, so no safe borrow can survive the
move. Its pointer read transfers exactly one initialized, correctly aligned T;
ManuallyDrop prevents KBox destruction from dropping that value again. The
Storage guard only releases memory, using the same sized layout as construction,
and never dereferences the moved-from slot. ZSTs skip deallocation. This extraction
exposes no raw pointer or allocator injection and is not a pinning operation.
The returned value's destructor runs in its eventual owner's context, not during
extraction. Tests cover single destruction and preserved payloads; they do not
force global OOM or establish that future task/signal migration is failure-safe.

## Fallible work callbacks

Callback allocation failure drops captured values without invoking the callback.
Consuming a reusable callback destroys its captures exactly once after invocation.
Borrowed captures remain bounded by their original lifetime. Tests cover generic
Fn/FnOnce bounds, erased FnMut state, and injected failure with KERNEL/NOWAIT.
These are local allocator tests, not global OOM injection or proof that nested
allocations inside callbacks are fallible.

## Fallible directory iterators

Iterator forwarding preserves the underlying iterator's execution-context
requirements. Allocation flags neither constrain next/nth nor the iterator's
destructor. Failed allocation of a chained iterator drops both input owners;
private allocator-injection tests cover this cleanup without exhausting memory.

## 边界与不变量

输入是有效 Rust 值和可信内核调用方声明的分配策略，没有用户裸指针。
类型不能证明调用方真的处于可睡眠上下文，调用方必须审计锁和中断状态。
底层 `kalloc` 必须返回满足布局且独占的存储，并支持跨 CPU 释放。
`new` 默认允许睡眠，即使当前后端没有回收也不能在禁止睡眠的路径使用。
`new_with_flags` 拒绝未知位；所有已知位都是回收许可，不要求后端执行它们。
无 `DIRECT_RECLAIM` 时不得睡眠，IO/FS 位不能绕过这个限制。
NOIO/NOFS 只约束这次申请，不建立隐式的跨调用链回收作用域。
两个策略都允许睡眠，不能用于替代 NOWAIT。即使持有的是可睡眠锁，也
必须检查回收依赖，避免回收等待调用方已持有的锁。接口不会自动检测
当前上下文或降级策略。NOWAIT 不承诺 NMI 安全或任意锁下的可重入性。
失败会立即析构输入值；最终 Drop 在释放方上下文执行且不继承分配 flags。
调用方必须分别保证这些析构路径允许在各自上下文运行。

`src/kbox.rs::new_with_allocator` is a private audit boundary: production passes
only `allocate`; test substitutes only return failure. Any successful substitute
must satisfy the same layout, exclusivity and allocation-origin requirements. Allocator
injection remains private; raw ownership recovery has its separate unsafe contract.

## Unsafe 清单

| 位置 | 不变量及约束 |
| --- | --- |
| `KBox::new_with_allocator` | 后端提供有效对齐的布局存储，ZST 使用对齐 dangling；写入前不创建引用 |
| `Deref` / `DerefMut` | 完整初始化后才能构造 KBox；引用生命周期受所有者借用限制，写访问要求独占借用 |
| `KBox::drop` | Compute the dynamic layout while the value is valid; the stack-local Storage guard frees storage after destruction or unwinding, without accessing the destroyed value |
| `KBox::into_inner` | A consumed sized owner supplies initialized aligned storage; ManuallyDrop prevents duplicate destruction, and Storage frees only the original allocation after the move |
| `Send` / `Sync` | 仅允许符合对应 trait 的值跨线程移动或共享；内部指针不暴露 |

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
| --- | --- | --- | --- | --- |
| T-01 | 内存压力导致 fatal OOM | 中 | 可满足布局但后端资源不足 | 直接调用可失败 kalloc 接口，返回 KError::NoMemory，不经过 GlobalAlloc fatal 路径 |
| T-02 | Memory corruption from an incorrect layout or double free | High | Corruption of private storage provenance or DST metadata | Private fields and no Clone/raw API; compiler-generated unsizing preserves provenance, and the dynamic layout matches the original allocation |
| T-03 | 后续回收在不可睡眠上下文阻塞 | 高 | 错用默认 new 或忽略 NOWAIT | 非睡眠调用点使用 new_nowait；未来分派必须遵守 flags；当前后端不执行睡眠回收 |

## FMEA

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| F-01 | 分配失败 | 内存不足 | 输入值释放，无 owner 返回 | 调用方决定传播或重试 | 3 | 错误返回；局部失败注入验证析构 |
| F-02 | VFS 上下文初始化失败 | 内存不足 | 无上下文发布、无锁安装 | 当前 lock/flock 返回 ENOMEM | 3 | Once 重置，可重试；成功后复用 |
| F-03 | 值构造或析构自身失败 | T 的外部逻辑 | 不由 KBox 转换成 KError | 仍可能内核 panic | 2 | 明确失败保证仅覆盖对象存储申请，不承诺完整 OOM 安全 |

## Reserved storage initialization

AsRef/AsMut delegation introduces no raw pointers or extended lifetimes. Debug
borrows the pointee and may do whatever its formatter does; allocation flags do
not make formatting safe under arbitrary locks. A slice regression verifies
borrow identity, mutation and delegated debug output.

The stored value before initialization is a valid `MaybeUninit<T>`, never an
uninitialized `T`. Its size and alignment equal those of T, including aligned
ZSTs. `KBox::write` first initializes the slot through exclusive safe access,
then suppresses destruction of the old owner and transfers its pointer to the
initialized owner. No reference to T is formed before initialization and the
allocation is released exactly once using the unchanged layout. Overwriting an
already written MaybeUninit slot does not drop its former value, matching the
underlying type's contract. Tests cover address/alignment preservation, single
destruction, abandoned sized/ZST reservations, invalid flags, and local allocator
failure. These tests do not force global OOM.

## Runtime-length slice boundary

Layout::array checks size overflow before allocator entry. Slice metadata is
created from the validated element count and an aligned pointer; MaybeUninit<T>
has the same size/alignment as T. The unsafe assume_init API requires every
element to be initialized; partial writers must destroy their initialized prefix
before abandoning the reservation. Dropping uninitialized storage never drops T.
Copied initialization cannot panic or run user code. Dynamic drop computes the
original layout before dropping elements and frees storage with that layout.
Local tests cover overflow, flags, allocator failure, alignment, empty/ZST
metadata, address preservation and element destruction; not global OOM.

ZST layout validation cannot limit element counts through byte-size overflow.
Copy ZST filling explicitly skips element iteration, and Copy types have no
destructor. Tests cover an aligned Copy ZST with usize::MAX elements and verify
that flags are still validated. Initialized non-Copy ZSTs may have observable
destructors: those must run once per element, as a separate small-count test
checks. Callers must bound untrusted counts and per-element work; the allocator
does not promise bounded initialization or destruction latency. An arbitrary
global length cap would not bound the cost of user-defined destructors.

The reference comparison in design.md separates byte-layout safety from work
limits. Linux Vec drop explicitly destroys its initialized slice, including ZST
destructors; this must not be optimized away based on allocation size. Likewise,
generic Clone or initializer callbacks may have side effects even for ZSTs.
Only the current Copy-fill API can skip element writes without skipping user
code. No generic all-zero constructor is provided: zero bytes are not valid for
arbitrary T, unlike copying an already valid value.

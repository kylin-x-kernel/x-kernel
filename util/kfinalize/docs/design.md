# kfinalize — 设计文档

## 定位与范围

公共 `DeferredArc<T>` / `DeferredWeak<T>` 将末公开强引用与 payload 析构分开。只提供普通可睡眠 worker 析构，不提供 BH 析构类型。普通使用者仅依赖 kfinalize；本 crate 不依赖 ktask、kirq 或 kworkng。安全构造器仍要求 provider 已安装、当前可睡眠；受审计的启动/任务准备通过 unsafe service::new_integration 提前准备。此入口复用现有共享链表保活未绑定责任，不扩展到 Task/RCU 迁移或公共排空 API。

隐藏集成层的 unsafe service::new_integration 从已安装 provider 获取新鲜 carrier，复用同一 identity/Box 构造。主 CPU 串行安装后，各 CPU 在本地基础能力就绪时可并发准备；未安装返回 NotReady 与原值。既不更改 Arc 计数，也不保活公开强引用。早期末强引用释放后 Weak 立即失效；服务成功绑定后才执行业务析构。启动失败时永久 inbox 保留责任，不承诺失败启动后的释放，也不允许启动线程等待回收来解决服务自身的启动依赖。

## 使用原则：非必要不使用

默认保留普通 Arc 和直接析构。只有明确存在“最后引用可能在不允许该对象析构的上下文释放”的需求，才考虑 DeferredArc；不能仅为了统一类型或省写回调而全量替换。

- 接入时说明最后引用可能出现的上下文、完整析构链为何不能在那里执行，以及普通 worker 上下文如何满足要求。
- 已有安全、正确的直接释放路径无需改造；RCU 宽限期后的 BH 回调已经可以直接析构时，不额外套延后析构类型。
- 评估对象数量、创建/销毁频率、积压驻留内存和共享 worker 依赖；大量小对象尤其不能忽略每对象固定元数据。
- 异步只提供执行上下文转换，不保证更快，也不保证更低尾延迟；同样不承担引用之外的 RCU/设备生命周期协议。

当前仍是每对象三块分配：带 NMI 退休前缀的 identity、带 raw-owner
头部的 payload、带 NMI 节点的 carrier；ZST 也有 payload 头部分配。
64 位 DeferredArc/DeferredWeak 句柄各为 24 B，普通全局 allocator 的
Arc/Weak 为 8 B。Clone 不新建 carrier，但解引用多一层指针间接访问，
末引用释放增加队列同步、通知和 worker 调度，NMI 再增加普通 IRQ 转交。
当前完整 allocator 请求量、slab 占用及 Task 总体增量尚未重新计量，
不能引用历史版本的测量作为当前采用依据。大规模接入须针对目标配置补测。

接入评审必须记录：析构上下文冲突、对象峰值数量与创建/销毁频率、
每对象及积压内存预算、共享执行器等待依赖，以及模块自己的完成/关闭协议。
类型约束和构造器检查不能自动证明这些业务条件；不满足时保留普通 Arc，
或先调整模块生命周期，而不是先替换再假设后台能够及时释放。

## 架构

```text
DeferredArc ── Arc<Identity<T>, IdentityAllocator> ── Box<Payload<T>> (raw header + T)
                     │
                     └── Arc<dyn Prepared> ── 轻量链表节点 ── 共享 Normal Work
DeferredWeak ── Weak<Identity<T>, IdentityAllocator>
```

`src/lib.rs` 拥有公开身份与 payload；`src/service.rs` 是给内核集成层的隐藏 provider 契约。kworkng 启动早期安装集成准备入口，执行服务初始化成功后安装普通 Normal 准备入口；二者共用同一 executor，每次返回独立载体。普通构造在普通入口未安装时直接返回 NotReady，不因集成入口已安装而绕过上下文检查。

没有发现需要“末引用自动触发 BH 析构”的业务消费者，因此首版移除 BH 入口。RCU 已有的非睡眠析构回调保持原样；将来只有具体消费者证明独立需求时才重新评估。不睡眠不等于任意上下文安全，但这个区别本身不是新增通用类型的理由。

## 调用约束 / 执行上下文

| 操作 | 允许的上下文与前提 | 不提供的保证 |
| --- | --- | --- |
| 普通 new | provider 已安装的可睡眠任务上下文 | 不允许 IRQ/NMI 中临时构造 |
| unsafe service::new_integration | provider 已安装，各 CPU 本地 allocator/per-CPU/IRQ 锁就绪；已审计启动/任务准备，可禁止抢占 | 不允许 hardirq/softirq/NMI 构造；不是通用检查旁路 |
| 最后强引用释放 | task、softirq、hardirq、已审计启动路径；NMI 要求本 CPU irq_work 已准备好 | 普通路径锁序约束不变；不保证 wait-free |
| 最后 Weak 释放 | 普通上下文直接释放 identity；NMI 经独立 irq_work 节点转交普通 IRQ | 不支持未准备 irq_work 的 NMI；不保证延迟 |
| payload Drop / Box free | 普通可睡眠 worker，链表锁外 | 不在提交者现场执行；不保证回收时延 |

NMI 分支在任何普通 guard、pending/queue 锁或 allocator free 前判断。
payload carrier 的原始 Arc owner 转交到普通 IRQ，之后沿原链表进入 worker。
控制块使用标准 Arc 的 allocator_api：provider 在请求区域之外准备独立
退休头，末 Weak 的 allocator deallocate 仅交出原始分配 token。普通路径
不增加 IRQ 跳数。不支持或未就绪的 NMI 交接按致命契约违规处理，不能
退回到 NMI 内直接释放。Weak upgrade 得到的临时 owner 遵守同样条件。

这不等于禁止 NMI 读取某对象：若模块另有协议，保证 NMI 外的 owner
覆盖整个借用期间，且读取操作本身适合 NMI，可以单独审计该借用。
DeferredArc 不自动提供此借用协议，也不保证任意 T 的方法适合 NMI。

创建由 provider 检查可睡眠任务上下文及就绪状态，失败带回原输入。OOM 沿内核策略。Clone/Deref/Weak 沿标准 Arc 语义；NMI 末 Drop 额外要求本 CPU irq_work 已初始化。提交不分配载体、不等待后台、不内联 payload Drop；不承诺 wait-free 或持任意锁安全。

IdentityAllocator 是每 identity 一次分配的 allocator，不是通用多分配池。
成功分配后才能 Clone，复制永久 policy 和原始 token；所有副本拒绝第二次
allocate。Arc 构造成功后才产生这些副本，析构 allocator 本身不释放任何
资源。token 独立保存原始 provenance，不能从 Arc 缩窄后的区域指针反推
整块分配再 free。公共 handles 因而包含 16 B allocator 状态（64 位）。
默认 Global policy 只供安装前的内部测试；生产 provider 必须先安装内存
policy，早期对象保留创建时的 policy，不会随后切换 deallocator。

Normal 最终 Drop 可以睡眠，但仍须避免共享池等待依赖环。即使最后引用在 softirq 中释放，也只提交到普通 worker，不在 softirq 内析构。关机前的落盘、设备停止仍由所属模块同步保证；本机制不是关机屏障。

共享 Work 使当前普通回收串行：某个 Normal 析构睡眠会阻挡后续 Normal 节点。因此析构不得直接或间接等待同一执行器中其他对象完成回收；允许入队新的对象但不等待它。这是共享方案新增的接入限制，不是额外线程能自动解决的问题。

## 状态机与算法

1. 检查 provider，准备 carrier；任何可恢复失败都尚未接管输入。
2. 分配稳定 Box<Payload<T>>，建立标准 Arc 公开身份，发布前初始化 payload 头部。布局三块分配：identity、payload、carrier；即使 T 为 ZST，payload 仍须分配头部。
3. 标准 Arc 将最后强引用变为零后调用 Identity::drop。此时所有 Weak 升级失败。
4. Identity 交出唯一 Finalization；载体保活不引用公开身份，不延长 Weak 升级窗口。
5. 准备时固定的执行器消费 Finalization，Box::from_raw 执行 T::drop 并释放 payload 分配；不根据提交上下文切换执行器。

## 并发模型与设计决策

### 调度所有权接口

`as_ptr` 只借用 T 地址；`ptr_eq` 比较 identity；`strong_count` 只是瞬时
观察，不能代替同步。`into_raw/from_raw` 转交/恢复一个强引用，不增减
计数、不分配。每个 raw owner 必须恢复一次；遗漏则保留强引用而泄漏，
重复恢复属于调用者的 unsafe 违约。raw owner 存在时 Weak 仍可升级。
`borrow_raw` 专供 current 一类非拥有型视图：返回 ManuallyDrop 包装，
不消耗原始 count、不增加引用。调用者保证真实 raw owner 覆盖视图及其
借用，不能显式 Drop/取出包装中的 handle；clone 才产生独立拥有型引用。

payload 在 T 前保存不可变 RawIdentity：原 Arc identity 指针与兼容的
IdentityAllocator 副本。初始化发生在首次发布前，不添加保活引用，
不是引用环。into_raw 从完整 payload 原始指针投影 T；from_raw 仅接受
保留该分配 provenance 的拥有型指针，按 repr(C) 字段偏移回到头部。
不能从 Deref 产生的 &T 逆推头部；as_ptr 本身也不授予一份强引用。
这与禁止从 Arc 缩窄区域指针恢复 allocator backing allocation 不冲突：
此处保存的是 Box 完整分配的原始 provenance，未从共享 T 引用扩大权限。

末强引用结束后头部中的 identity 指针可能悬垂，worker 绝不读取它；
MaybeUninit 头部无需析构，其 allocator 副本本身不释放资源。worker 仅
析构 T 并释放整个 payload Box。末 Weak 仍按原 identity-memory 协议释放。

64 位 RawIdentity 为 24 B；usize payload 请求从 8 B 变为 32 B，ZST
从无 payload 分配变为 24 B，其他类型还受对齐影响。非 ZST 的分配次数
不增加，DeferredArc/Weak 句柄大小仍为 24 B。实际 Task 成本待接入测量；
不把宿主正确性测试当成正式性能数据。

公开强/弱引用竞争完全复用标准 Arc/Weak。公开引用存在时 payload 只有共享访问；只有公开强引用消失后的唯一 token 可取得析构权。没有自制引用计数，没有扫描或重试。

Prepared 的消费式 Arc receiver 与待处理链表持有的 Arc 分别保护提交者和执行者；worker 可先完成 payload 析构，提交者仍能安全返回。公共身份、carrier 与 payload 分开，因此该竞争不要求 payload 留到发布返回。链表锁仅保护发布/移除；锁外通知共享 Work、析构和释放节点，每轮最多处理 64 项，剩余由后续执行轮次承接。

## Drop / 资源释放

| 对象 | 释放位置 |
| --- | --- |
| T 的 Drop 和 Box<Payload<T>> 分配 | 普通 worker 的 Finalization::finish |
| Arc 公开控制块 | 最后强/弱引用的调用者；可能在 IRQ 路径 |
| 空 carrier 分配与元数据 | 最后发布者或 worker；可能在 IRQ 路径 |

后两行不能包含需要睡眠的业务析构，不声称全部 allocator free 被异步化。这是源码所有权边界，不是 allocator 逐次跟踪测量结果。

## 使用

依赖 `kfinalize.workspace = true`，在合法任务上下文调用 `DeferredArc::new(value)`。初始化由 runtime/kworkng 完成，使用者无需创建或管理 WorkHandle。没有 get_mut、try_unwrap、任意队列选择或取消最终析构接口。

RCU 宽限期仍由 RCU 负责，本 crate 不改变 RCU 调度底座。已经在 GP 后 BH 回调中可安全释放的对象无需为了使用本类型再排一次 BH；需要睡眠的析构可在 GP 后交接 Normal。

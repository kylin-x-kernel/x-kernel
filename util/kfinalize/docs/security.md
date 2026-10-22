# kfinalize — 安全与可靠性分析

## 信任模型与外部边界

输入是内核 Rust 对象 T: Send + Sync + 'static，不直接接受用户指针、MMIO、DMA、固件或网络字节。若 T 包含硬件/RCU 裸访问，所属模块必须先结束那些未计入引用的访问；引用归零不能代替 GP 或设备停止。provider 是受信任内核集成代码，不是用户可注入执行器。

## unsafe 清单与内存安全不变量

- `lib.rs::Identity` Send/Sync：标准 Arc 保证共享访问阶段；T 的 Send/Sync 允许跨 CPU 共享及最终移交。
- `DeferredArc::deref`：公开强引用保护初始化完成的稳定 Box；不暴露可变引用。
- `DeferredArc::from_raw/borrow_raw`：原始分配 provenance、未消费的强引用和初始化后不可变的 payload 头部共同保证恢复；borrow_raw 视图不得消耗计数，真实 owner 必须覆盖全部借用。
- `Identity::drop`：仅一次提交，所有公开 payload 借用已结束。指针及 destructor 类型配对由构造器维护。
- `destroy_value`：唯一 Finalization 恢复唯一 Box，执行一次析构和释放。
- `service::Finalization::finish`：调用者保证普通可睡眠 worker 上下文，且不在发布调用内执行。token 不可克隆；丢弃它只泄漏，不错误内联析构。
- `service::Prepared` / `install_normal`：unsafe provider 负责一次交接、永久服务、发布/执行保活、无取消、无提交分配或睡眠。安装仅用于串行启动。
- `service::new_integration` / `install_integration`：主 CPU 串行安装永久 provider，各 CPU 独立满足本地分配/锁前提后可并发准备，包括已审计 NoPreempt 路径。provider 返回新鲜 carrier，未绑定时保留责任；不创建任务、不等待执行。未安装返回原输入。此 unsafe 入口不允许 hardirq/softirq/NMI 构造，普通安全构造检查不变。

## 线程安全

公开 identity 不被后台保活，Weak 不能复活退休对象。T 的共享访问结束与标准 Arc 最后 Drop 同步；队列负责把 token 写入同步给 worker。载体单独保活，不能从 payload 推导载体仍存活或反之。核心不暴露 Finalization 构造函数及 payload 可变访问。

## 威胁分析与故障管理

| 风险 | 影响 | 防护及剩余约束 |
| --- | --- | --- |
| 错误上下文构造 | 原输入提前析构 | 返回 (CreateError, T)，调用者负责处理原值 |
| Weak 在队列等待中复活 | UAF/双重释放 | 公开 Arc 与私有 carrier 分开 |
| worker 先于提交返回 | 载体 UAF | provider 必须保留独立发布者 owner |
| 错误 provider 内联执行 | 原子上下文睡眠 | unsafe 安装及 Prepared 契约；普通用户不可选择 provider |
| Clone/Weak 改变执行策略 | 错误上下文析构 | 原 carrier 固定队列，Weak 升级仍只返回 DeferredArc |
| OOM 或不可能的接纳失败 | 致命停止/泄漏 | 沿内核策略；禁止在失败路径错误回收可能已发布的 token |
| 析构 panic | 泄漏或内核停止 | 不重试析构，不提供恢复承诺 |
| 业务等待形成共享池或同一 finalizer 的等待环 | 无进展 | 每类共享 Work 串行；析构不得等待同类其他对象回收，使用者审核间接依赖，本层不做全局死锁检测 |

## 隐私与限制

NMI 支持同时覆盖最后强引用的 carrier 交接和最后 Weak 的控制块释放。
两者有独立预备节点和独立寿命；NMI 不得进入普通锁或 allocator。
原始分配 token 独立保存在 allocator 副本中，禁止从 Arc 区域指针扩大
权限访问/释放前缀。provider 必须在准备对象前安装永久内存 policy。
NMI 交接要求本 CPU irq_work 已初始化；失败保留原始 token 后致命报错，
不内联析构或用普通提交路径降级。NMI 中访问 T 仍须保证其操作本身合法。

不记录 payload 内容；可能保留业务对象直到 worker 有机会执行。不提供回收延迟上界、不支持平台的 NMI 回收、全部 free 异步化、强制关机排空或引用环收集。

## 审计清单

raw 所有权转交新增 payload 头部审计点：它在发布前完整初始化，之后
只读；保存非拥有型 identity 指针和不执行释放的 allocator 副本。raw
owner 自身保留 Arc 强引用，保证恢复时 identity 仍活着。只允许从
into_raw 原始指针保留完整 provenance 的偏移往返，禁止从 &T 构造
拥有型指针或读取头部。恢复次数与转交次数严格一一对应，不允许重复
消费。最终 worker 不读取可能已悬垂的 identity 字段。Miri 测试覆盖
侵入式字段偏移往返、共享 T 访问交错、高对齐、ZST 和 NMI 恢复后释放。

- 最后强引用与 Weak 升级是否仍全部走标准 Arc？
- token 是否只来自已经终止的公开身份，且 destructor 与分配类型匹配？
- provider 是否只准备一次、提交一次，且保持执行上下文及发布期间存储？
- 元数据析构是否仍无业务睡眠行为？
- 构造失败是否返回原输入，而非隐式丢弃？

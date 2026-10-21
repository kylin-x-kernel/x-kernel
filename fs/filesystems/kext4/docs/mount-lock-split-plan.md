# KExt4 普通文件回写与同步路径的大锁拆分交付计划

日期：2026-10-20。状态：PR 1 本地实现及验证完成，待远端评审；PR 2–4 尚未实施。

本计划按四个顺序依赖的 PR 交付。基于当前 `kext4/checkpoint-dedup` 分支的
`551115f85` 梳理；正式开始时重新固定源码、配置和测试基线。
历史 `4270ac317` 的共享 mount 锁方案及其诊断数据仅作为设计输入，不直接沿用其性能结论。

## 1. 总体任务与边界

### 1.1 总体任务

解除普通文件稳定映射覆盖写、数据回写及其同步提交对 mount 全局串行化的依赖。
将映射有效性、元数据修改、事务推进、缓存生命周期分别交给对应的保护机制，
使不同文件能够独立推进，只有实际共享资源或持久化顺序产生依赖时才相互等待。

这里的“整个任务”指大锁拆分的第一阶段，四个 PR 共同完成这一阶段，
不等同于把整个 KExt4 的所有修改路径全部改为细粒度并发。
本期完成后应准确描述为“普通文件稳定映射回写与同步路径解耦”，不能宣称“KExt4 大锁已全部拆完”。

### 1.2 范围分层

| 层次 | 本期范围 |
|---|---|
| 性能改造主线 | 已初始化、已分配、不需要推进磁盘大小的普通文件 buffered 覆盖写、writeback、fsync/fdatasync |
| 必须完成的基础 | 同 inode 回写/映射有效性保护、共享元数据块完整读改写、事务依赖及提交协调、缓存引用与回收保护 |
| 必须适配或验证的交互 | 普通读取、truncate/释放块、属性/xattr 更新、创建/删除/rename、final eviction、credit restart、syncfs、现有卸载同步入口 |
| 可保留粗粒度执行的路径 | 分配新块、增长、unwritten 转换、目录与 orphan 修改；但必须参加与新路径共同的元数据、事务及映射保护协议 |
| 不作为本期交付 | 全部分配器按组并行、目录操作全面并行、完整后台 writeback/commit/checkpoint、精确 inode sync/datasync tid、通用 shrinker、完整 freeze 新接口 |

新旧路径必须使用相同的底层保护机制。旧路径保留 mount 写锁，不能作为它与已绕开
mount 锁的新路径互斥的证明。必要的跨模块配套修改属于本期，不能因原先文件范围估小而省略。

### 1.3 最终完成条件

1. 稳定映射数据 I/O 不持有覆盖整个文件系统普通操作的 mount 读锁或写锁；
   mount 级生命周期准入可以保留，但正常事务提交不能借此独占排斥所有数据回写。
2. 元数据发布、提交和 checkpoint 使用明确的内部协调。允许必要的 journal 持久化串行，
   不允许用一把覆盖普通操作的全局锁替换原 mount 锁后声称完成拆分。
3. 所有影响上述路径的旧操作已适配共同协议，映射、元数据、完成前缀和恢复不变量成立。
4. 独立文件并发 fsync 的平均延迟和 P99 获得可重复改善，并同时报告完整 write+fsync
   周期吞吐；单任务和共享文件的回退必须得到解释和处理。
5. 所需单测、guest 回归和针对变更边界的失败/恢复验证完成，设计、安全文档与 rustdoc 同步。

## 2. 当前基础与设计约束

当前 `Ext4Mount` 在 `vfs/mount.rs` 中包装 `RwLock<Ext4SbInfo>`。
除访问串行化外，guard 退出时还驱动缓存维护；allocator 的预取至权威重读窗口也依赖
mount 保护。inode、buffer、allocator、journal 已有内部锁，但不代表跨调用的操作已具备完整并发契约。

必须针对现有代码消除以下隐含依赖：

- `MetadataWriteAccess::update_bytes` 的快照、修改、替换必须有完整并发保护，不能只证明
  每次读取和替换分别加锁。不同 inode 可能共享 inode-table 或其他元数据块。
- `JournalHandle::flush_ordered_data` 当前依赖 mount 写锁排斥并发写入后清除数据依赖。
  新协议必须区分本次屏障覆盖的依赖与之后新增的依赖，失败不清除。
- commit 冻结与活跃 handle 的退出必须能相互推进；内部容量/冻结等待不得直接作为
  正常并发下的新用户可见失败，也不能在等待时持有完成方需要的锁。
- checkpoint 选择、home 写回、tail 更新及退休目前依赖 mount 串行。迁移后仍须保持
  FIFO、frozen 版本、revoke、ring spare、recovery evidence 与“持久化成功后才释放空间”。
- 缓存槽位不能在 allocator 预取和正式修改之间被回收并重建出第二份身份；
  frozen image、脏 buffer、活跃访问及 I/O 所需对象的寿命都需要显式保护。
- 部分成功、PageCache 完成判定和元数据失败必须一致，不能把所有元数据发布简单挪到
  pass 尾部，也不能提前清理实际尚未完成的 dirty 范围。
- 不通过 `unsafe`、绕开 Rust 借用规则或复制第二份 mount/journal 身份实现并发。

Linux v6.12 可作为职责和语义参考：inode 与映射、元数据 buffer、journal、分配组分别协调；
不是照搬锁名称或照搬某个锁的持有范围。具体锁顺序必须结合 KVFS/PageCache 的现有调用关系制定。

## 3. 四个 PR 的交付链

```text
PR 1：共享状态、缓存与映射的保护基础
    ↓ 先使每份状态具备可验证的内部保护，外层大锁继续保留
PR 2：事务、持久化及回收的独立协调
    ↓ 解除 journal 协议对外层串行化的语义依赖，生产入口暂不全面放开
PR 3：稳定映射数据回写离开全局临界区
    ↓ 接入真实并发数据路径，旧修改路径遵守共同协议
PR 4：同步提交解耦、全链路验证与默认启用验收
```

四个 PR 是四个逻辑交付单元，不按固定行数裁剪。每个 PR 可以包含多个提交，
机械重构提交与行为变化提交分开，测试随对应行为变化交付。
后续 PR 基于前序结果开发并顺序合入；每个前缀都必须可构建、可运行、语义完整。

| PR | 核心问题 | 合入后可声明的结果 | 性能预期 |
|---|---|---|---|
| 1 | 大锁取消后，谁保护共享状态及其生命周期？ | 内部保护协议和相应测试具备，生产仍可保持串行 | 不承诺提速，不接受明显无解释回退 |
| 2 | 提交、flush、checkpoint 如何在并发修改中保持正确？ | journal 协调不再把 mount 大锁当作正确性前提 | 主要是能力建设，不承诺端到端收益 |
| 3 | 不同文件的数据 I/O 能否独立推进？ | 稳定映射数据 I/O 已脱离 mount 全局临界区 | 检查并发信号，同时记录未解耦的提交等待 |
| 4 | fsync 是否仍在全局提交入口排队？ | 本期完整路径及兼容路径闭环，性能和正确性验收完成 | 最终端到端收益判断 |

## 4. PR 1：建立共享状态与生命周期保护基础

建议标题：`refactor(kext4): establish buffer and inode concurrency boundaries`

### 任务

1. 建立本期路径的状态保护表和锁/等待关系图。逐项标明现有内部锁、仍依赖 mount 的
   操作、迁移后保护者，以及是否允许设备 I/O、等待、回调和重入。
2. 完善元数据块写访问协议，使快照到发布的完整读改写不会丢更新。
   协议同时约束新回写、旧 metadata mutation、事务冻结和 checkpoint 使用的版本。
3. 为回写与映射修改建立共同的文件级保护。优先复用 inode 回写协调及现有 invalidate
   语义；说明保护起止点，覆盖 truncate、释放和 final eviction，不默认 inode state 锁等价于映射寿命保护。
4. 给缓存预热、权威读取、修改、回收建立显式引用/访问寿命约束。
   可以保留有界回收入口，但其安全性不再以“所有 allocator 都在 mount 写 guard 内”为最终前提。
5. 将上述协议接入所有相关旧调用者；本 PR 不提前放开生产数据回写并发。
   只为拆分访问准备必要的接口，不建立无人使用的通用框架或第二份状态。

### 主要落点

`buffer/access.rs`、`buffer/slot.rs`、`buffer/cache.rs`、`inode.rs`、`vfs/mount.rs`，
以及需要配套的 truncate、allocator 和 metadata 调用者。文件列表用于导航，不构成修改上限。

### 验收与交付

- 两个更新者修改同一元数据块的不同字段，结果都保留；失败更新不发布半成品。
- 缓存回收与预热/修改交错，不产生重复身份、过期快照覆盖或冷读进入禁止阻塞的临界区。
- 映射保护的等待关系可确定地验证，无读写锁升级、递归加锁或逆序等待。
- 现有功能通过；生产仍可依赖旧入口串行，不以本 PR 宣称大锁已拆除。
- 随 PR 更新 `design.md`、`security.md` 和所触及 API 的锁及寿命契约。

本 PR 合入后的可回退点：外层 mount 锁仍然完整，后续路径尚未依赖并发开放。

## 5. PR 2：建立 journal 自身的持久化协调

建议标题：`feat(kext4): coordinate journal progress independently of mount exclusion`

依赖 PR 1 的 buffer 修改与版本协议。

### 任务

1. 明确 running、冻结、committing、pending checkpoint 的协调者，以及提交/空间等待接口。
   冻结先封闭相应事务的新修改准入，再等待已有修改退出；等待者不持有退出所需的锁。
   对重复提交请求、并发空间请求和 abort 提供一致结果。
2. 重新定义 ordered-data 的登记、屏障和确认。对可变事务中的 flush，使用明确的
   依赖快照/代次或等价协议；不能让一次 flush 清掉屏障覆盖范围之后的新依赖。
   冻结事务的依赖集合必须与日志提交使用的 frozen 内容一致。
3. 将持久化组织为“受保护地选择/准备 → 执行 I/O → 受保护地确认/发布”。
   内部短状态锁不贯穿设备 I/O。允许提交执行权或 checkpoint 执行权保持必要串行，
   但它们不应阻止无关文件数据阶段进入。
4. 接管 append/checkpoint 的互斥、日志空间预留及 tail 退休协议。
   当前预算、FIFO 和持久化顺序保持，不能为了拆锁顺手加入新的回收策略。
5. 处理首次 journal feature/recovery flag 初始化、共享 superblock 元数据和
   `PreserveDuringRecovery` 等特殊路径，避免旧 mount 排他假设残留。
6. 适配所有 journal 使用者和失败路径；生产调用者暂时继续保留外层 mount 保护。
   `&mut self` 只有在实际可变状态已交由正确所有者保护后才调整。

### 主要落点

`journal.rs`、`jbd2/transaction.rs`、`jbd2/commit.rs`、buffer commit/checkpoint 接口，
以及容量 restart、元数据更新完成和 recovery-feature 处理路径。

### 验收与交付

- 正常并发冻结/提交能等待推进，不把内部 `JournalBusy` 竞争直接变成新 syscall 失败。
- 屏障期间新增数据依赖不被错误清除；flush 失败、abort 和重试保留正确状态。
- 并发提交不重复持久化、不重复退休、不覆盖未回收日志，不破坏恢复标志与 frozen 版本顺序。
- 小 journal 强制覆盖 JournalBusy、credit restart、前缀 checkpoint 及其失败分支。
- 在不借助生产外层 mount 锁的内部测试中验证协议；短锁和睡眠执行权的职责有清晰文档。
- 同步更新事务状态机、等待条件、错误传播及 recovery 不变量文档。

本 PR 合入后的可回退点：生产尚未全面去掉 mount 锁，协议变化已有独立验证；
后续性能入口可以独立关闭或回退，而不撤回底层正确性能力。

## 6. PR 3：启用稳定映射数据回写并发

建议标题：`perf(kext4): write stable mapped data outside mount-wide locking`

依赖 PR 1 的映射/缓存保护与 PR 2 的事务协调。

### 任务

1. 从 `Ext4Mount` 提供受生命周期约束的稳定访问上下文，使数据路径能使用必要的
   device、geometry 和映射信息，而不为借用 `Ext4SbInfo` 持有整个 I/O 期间的 mount guard。
   不复制可变 metadata 或 journal 身份。
2. 仅启用已初始化、已分配、无需扩容的覆盖写路径。全范围预检后才发数据，
   不满足条件时在发出任何数据写之前回退，避免半段新路径加整段旧路径重复提交。
3. 保留同 inode 必要排他和映射寿命保护；不同 inode 可同时推进设备数据 I/O。
   硬件队列拥塞属于真实共享资源，不承诺不同文件完全无等待。
4. 数据完成后通过共同元数据协议发布并登记 ordered 依赖，消费单次完成凭据；
   与 PageCache 的逐范围完成/失败语义一致。若仍需短 mount 访问，标明剩余范围，
   不在其内执行数据 I/O，也不依赖读锁升级。
5. 适配 truncate、eviction、属性更新、delalloc 结算和旧回退路径的共同保护。
   检查“新路径先拿 inode 保护、旧路径先拿 mount 锁”等交叉顺序，消除循环等待。
6. 不顺带实现数据请求合并、全局 RwLock 公平性修改或 pass 末尾集中元数据发布。
   这些若另有必要，应有独立证据与任务，不混入本期收益归因。

### 主要落点

`vfs.rs` 的 writepages 和相关访问入口、`vfs/mount.rs`、`file.rs`，
以及 inode、truncate、delalloc 与失败完成的配套路径。

### 验收与交付

- 在内部可控设备测试中暂停文件 A 的数据 I/O，证明文件 B 可进入数据提交；
  不以用户态毫秒阈值测试内部锁结构。
- 覆盖同文件并发、不同文件共享元数据块、回写与 truncate/释放交错、回退及数据失败。
- 对 FullMetadata/DataOnly 分别验证完成前缀与 reservation；元数据未成功时不提前清理数据。
- 运行三个核心 fio 场景，判断是否已出现真实并发，以及等待是否转移到发布或提交。
  报告剩余全局等待，本 PR 不替代最终验收，也不能掩盖像历史共享锁方案那样的明显延迟回退。
- 更新新旧路径、锁范围、完成凭据及回退条件文档。

本 PR 的回退以统一关闭新数据路径为单位。必须保证凭据已消费、在途 I/O 已退出；
不能在一次回写中途切换保护协议。是否采用临时开发开关由实现决定，不强制增加永久 Kconfig。

## 7. PR 4：同步提交解耦与阶段验收

建议标题：`perf(kext4): decouple fsync persistence from mount-wide serialization`

依赖前三个 PR。本 PR 包含最终的真实行为变化，不是将全部测试留到最后的汇总 PR。

### 任务

1. 将 `fsync`/`fdatasync` 的提交与设备等待接入 PR 2 协调接口，不再持 mount
   全局独占权等待设备。保留当前“保守提交相关时点的 running transaction/必要 flush”语义；
   不引入按 inode number 保存 tid 的全局表，也不承诺完成精确 inode tid 机制。
2. 处理所有可触发提交的调用者，而不只改显式 fsync：包括 credit restart、mutation
   完成、journal 空间压力、syncfs 及现有卸载同步。旧调用者也不能拿着其他完成方需要的锁等待。
   需要释放粗锁的旧操作，必须先到达可恢复的一致性边界；重新进入时按需重校验。
3. 将压力 checkpoint 的执行接入 journal 自身协调，保持数据、日志、home flush 和 tail
   的持久化顺序；不建设完整后台 worker。允许日志空间不足的提交者等待必要回收。
4. 接入现有生命周期准入和排空边界，确保卸载不销毁仍被访问的 state/device/journal。
   syncfs 在并发写入下有明确同步范围，不以“等待永远不再有新写入”作为完成条件。
   不把尚无公开入口的完整 freeze 功能扩成附加项目。
5. 清理本期路径残留的 mount 串行化和过时假设；列出仍保留粗锁的操作及其原因。
   纯结构调整与本 PR 的行为切换分开提交。
6. 完成阶段性能、语义和恢复验收，形成可复现报告，并据此决定最终默认启用。

### 主要落点

`vfs.rs` 的 fsync/sync_fs 与生命周期入口、`file.rs::sync_inode`、journal 提交/
checkpoint 入口，以及所有隐式提交调用者。必要的共享 KVFS 接口改动须说明契约，避免另建生命周期所有者。

### 验收与交付

- 可控测试证明事务持久化等待不再独占阻挡无关稳定映射的数据提交；
  活跃 handle、慢 I/O、提交与回收之间均能前进。
- 核心 guest 并发回归、失败注入和相关 powercut/replay 验证完成。
- 相同源码父提交、相同测试环境下的性能数据能支持本阶段收益，不以吞吐改善替代 fsync 延迟判断。
- `design.md`/`security.md`/rustdoc 与实际默认路径一致，明确剩余全局协调和未实现能力。
- 若性能目标未满足，记录未满足项并保留可用基础，不能以“代码已写完”认定任务完成。
  必要时回退启用行为，继续在本阶段定位，而不叠加无关优化。

## 8. 验证与性能交付要求

### 8.1 各 PR 的常规验证

遵循共享 build-workflow。每次切换平台后先从对应 defconfig 生成配置，不用裸
`cargo check -p` 代替构建。aarch64 主验证示例：

```bash
cp platforms/kplat-aarch64/qemu_defconfig .config
make defconfig
make build
make clippy
make clippy UNITTEST=y UNITTEST_CRATE=kext4
cargo +nightly-2026-03-08 fmt --all -- --check
git diff --check
```

执行 `make unittest UNITTEST_CRATE=kext4 DISK_IMG=...` 时使用独立、已准备好的测试镜像，
不得覆盖性能基线镜像或用户数据。涉及共享 KVFS/PageCache 时增加相关 crate 的针对性验证。
阶段结束至少补 x86_64 配置构建/检查；若执行环境不足，明确列为未完成项，不声称全架构通过。

用户态 ABI/并发契约测试放在外部 Starry Test Harness，优先扩展已有用例。
新用例先注册 `ci-test-iter`，性能用例使用 daily；不凭内部日志或调度时序断言 syscall 语义。
案例名在检查现有覆盖后确定，文档不预造尚不存在的 CASES。
内核内部确定性并发测试负责锁与状态交错，guest 测试负责用户可见结果。

### 8.2 必须覆盖的交互矩阵

| 交互 | 验证重点 | 首次主要交付 |
|---|---|---|
| 两个 inode 更新同一 metadata block | 无丢更新，正确版本和 checksum | PR 1 |
| 预取/修改与 cache reclaim | 唯一槽位身份、pin 寿命、无禁止临界区冷 I/O | PR 1 |
| 新增依赖与 flush 确认交错 | 新依赖保留、失败不清除 | PR 2 |
| active handle、冻结、并发提交与 abort | 有进度、等待无环、错误一致 | PR 2 |
| append、prefix checkpoint、tail 及 revoke | 不覆盖、不早退、不写错 frozen 版本 | PR 2，PR 4 集成 |
| 回写与 truncate/shrink/regrow/释放 | 无释放后写入、无旧数据暴露、正确大小 | PR 3 |
| 共享文件、独立文件及共享 metadata | 排他范围恰当、无交叉等待死锁 | PR 3 |
| 数据/metadata 失败与 PageCache completion | 前缀及 reservation 结算一致，dirty 可重试或按既有语义 abort | PR 3 |
| 隐式提交、syncfs、现有卸载同步与在途 I/O | 无持锁等待循环，无过早销毁、假成功 | PR 4 |
| 改动的持久化边界处掉电与 replay | 保留既有 durability/recovery 语义 | PR 2 起逐步覆盖，PR 4 完整集成 |

### 8.3 性能基线与报告

- 在实施前固定新的 A0 基线，记录 commit、构建身份、resolved config、rootfs、fio/job/
  prepare 哈希、QEMU 参数、加速方式、CPU 数、内存及设备配置。旧报告不能直接作为新 A0。
- 核心场景：随机 fsync64 单任务、四任务共享文件、四任务独立文件。
  guardrail 增加 fsync1、顺序 4 KiB fsync64，以及普通读写和混合 metadata 压力。
- PR 1/2 看父子版本正确性及无解释回退；PR 3/4 既比较直接父提交，也比较 A0 与最终版本，
  防止中间成本被后续报告隐藏。不同 main 基线不作单改动归因。
- 最终无插桩对照采用交错版本顺序，每版本至少五次独立 guest 运行；保持已有
  5 秒预热、30 秒正式窗口作为核心可比条件。尾部样本不足时增加独立长时场景并单独报告。
- 报告 write IOPS、平均/P99/P99.9 fsync、sync 样本数、观察最大值和完整 write+sync 周期。
  不把各调用的 P99 相加；每轮分位数与合并分布分开，最大值不作硬上界。
- 诊断用于解释 inode 等待、映射保护、metadata 保护、journal 排队、数据及 flush；
  不将重叠累计等待相加，不把插桩版本当最终收益证明。
- 在 PR 3 启用前冻结性能验收阈值：先测基线波动，再约定主场景最低改善和 guardrail
  可接受回退范围。不得看完候选结果再调整目标；本计划不预先承诺任意百分比。
- 主场景必须是 fsync 平均/P99 可重复改善且周期吞吐无实质损失。
  共享文件和单任务的异常回退未处理时，不宣称总体性能目标完成。

原始结果、解释脚本、配置、构建身份和诊断补丁保存在仓库外独立目录；
临时插桩不混入性能功能提交。新结果使用新标签，不覆盖旧证据。

## 9. PR 审查、交接与回退

每个 PR 描述都交付五项内容：

1. 本 PR 解决的具体问题，以及合入前后行为。
2. 状态归属、保护范围、等待条件及失败行为发生了什么变化。
3. 已完成验证的命令、结果和工件位置；未完成的检查单列。
4. 文档更新及仍依赖 mount 粗锁的操作清单。
5. 后续 PR 的输入契约与本 PR 的独立回退办法。

PR 1/2 不能靠“目前外层大锁还在”代替对新内部协议的直接测试；
PR 3/4 不能靠“底层已经有锁”代替全调用链集成验证。
已合入的 PR 1/2 可以保留为基础；回退 PR 3/4 时必须按依赖逆序，不能只恢复某个取锁点。

四个 PR 各自包含测试和文档，不另设第五个“补测试/文档 PR”。若某个交付单元过大，
优先在同一 PR 内拆清提交和审查章节，不能为了保持四个 PR 而接受中间不正确状态。

## 10. 参考资料与后续边界

- 当前实现：[设计文档](design.md)、[安全文档](security.md)、[mount 访问边界](../src/vfs/mount.rs)。
- 仓库规范：[代码规范](../../../../docs/ai/skills/code-guidelines/SKILL.md)、
  [构建验证](../../../../docs/ai/skills/build-workflow/SKILL.md)、
  [性能分析](../../../../docs/ai/skills/performance-analysis/SKILL.md)、
  [测试框架](../../../../docs/ai/skills/test-harness/SKILL.md)。
- Linux v6.12：[buffered write](https://github.com/torvalds/linux/blob/v6.12/fs/ext4/file.c#L266)、
  [ext4 inode/组锁](https://github.com/torvalds/linux/blob/v6.12/fs/ext4/ext4.h)、
  [fsync](https://github.com/torvalds/linux/blob/v6.12/fs/ext4/fsync.c)、
  [JBD2 状态与锁](https://github.com/torvalds/linux/blob/v6.12/include/linux/jbd2.h)、
  [VFS 锁规则](https://www.kernel.org/doc/html/v6.12/filesystems/locking.html)、
  [目录锁规则](https://www.kernel.org/doc/html/v6.12/filesystems/directory-locking.html)。

后续若启动全量大锁拆分，再单独定义 per-group 分配并行、目录/orphan 并发、完整后台推进
及生命周期建设的交付。本期不能把这些能力写成已经完成，也不以它们尚未完成为理由省略
本期数据路径必需的正确性基础。

# p9 — 安全与可靠性分析

## 信任模型

transport peer 和所有响应 PDU 均不可信。qid、目录项、属性、字符串、errno 和文件内容只有
通过 wire validation 后才能暴露给 filesystem caller。kernel caller、已建立的 transport
connection 和 allocator/scheduler 被视为可信。

## 外部边界 / 攻击面

- caller 提供的 name component、symlink target、open flags、offset 和 I/O length；
- transport 写入的 PDU header、payload、嵌套长度和服务端 errno；
- endpoint 断连、短响应、错误响应或永久不响应；
- final fid Drop 触发的同步外部 I/O。

本 crate 不访问用户指针、MMIO/PIO、DMA buffer、firmware、FFI 或架构 raw interface。具体
transport 对这些边界的处理不属于 `p9`。

## unsafe 代码清单

本 crate 没有 `unsafe` block、unsafe API 或自定义 `Send`/`Sync` 实现。

## 内存安全不变量

- response slicing 前验证 transport length、PDU declared size 和字段边界；
- read/readdir 的嵌套长度不得越过 payload，也不得超过 request count；
- write response 的完成字节数不得超过本次请求实际发送的数据长度；
- `P9Transport::request` 即使错误报告超出 buffer 的长度，也会在切片前被拒绝；
- `P9Fid` 持有 `Arc<P9Client>`，transport 不会早于 request/final clunk 释放；
- 每个 live fid number 在 client 内唯一，只有 clunk/remove 后才能复用。

## 线程安全

client 的 sleepable mutex 串行化 transport request、dialect/msize 和 tag/fid allocator。fid
的 qid 使用 mutex，open mode、iounit 与 live state 使用 atomic。锁顺序始终为 client state
后进入 fid qid；不存在反向路径。open 和 I/O 的状态检查、iounit 读取、请求发送都在同一个
client 临界区完成，不依赖额外的 opening 标志。所有这些锁只允许在可睡眠 task context 使用。

## 威胁分析

| 威胁 | 触发条件 | 影响 | 缓解 |
| --- | --- | --- | --- |
| 越界解析 | 短包或伪造 declared/string/data length | kernel memory safety | checked decoder 和嵌套长度验证 |
| response 混淆 | peer 返回错误 type/tag | 把其他响应当成本次结果 | 每个 request 验证 exact type/tag |
| partial walk | `RWALK` qid 数少于 names | 发布错误远端对象或泄漏 fid | 拒绝结果并 best-effort clunk 新 fid |
| fid 重复释放 | remove 后对象仍有 Rust 引用 | 重复 clunk server handle | remove 提交即设置 consumed live state |
| 跨 connection 操作 | link/rename 的 fids 来自不同 client | 请求引用无关 server handle | 发包前比较 client identity 并返回 `EXDEV` |

## 故障模式与影响分析（FMEA）

| 故障模式 | 局部影响 | 系统影响 | 处理 |
| --- | --- | --- | --- |
| dialect/msize 不支持 | negotiation 失败 | 该 mount 不发布 | typed error 并释放 client |
| malformed PDU | 当前 operation 失败 | 对象状态不提交 | `InvalidData` |
| server `RLERROR` | 当前 operation 失败 | caller 收到对应 errno | 保留 numeric errno |
| final clunk 失败 | server fid 暂时滞留 | endpoint 资源到断链才回收 | warning；本地 handle 标记 released |
| endpoint 永久不响应 | request task 阻塞 | 同 client operations 排队 | 当前无 timeout/cancel |

## 故障管理

协议和 transport failure 通过 `KResult` 传播。只有 Drop 无法返回 clunk error，因此记录
warning；不会把失败当作成功的 filesystem operation。partial walk 和 open failure 分别清理
新 fid、保持 unopened state。

## 隐私分析

文件名、metadata、symlink target 和文件内容会发送给配置的 endpoint。本层不提供传输加密，
机密性依赖所选 transport、VMM/host 和部署隔离策略。

## 已知限制

- 只协商 9P2000.L；
- 一条 connection 的 requests 同步串行；
- 没有 timeout、cancel、legacy/dotu、xattr 或 remote lock API。

## 审计清单

- 新 PDU 是否验证 type、tag、字段和嵌套长度；
- 新操作是否保持 Linux fid 角色和 wire 字段顺序；
- fid 是否可能在 live 时复用，或在 remove 后再次 clunk；
- 是否重新引入绝对路径 API 或把 filesystem state 放入协议对象；
- 新字段是否对应 Linux 语义或 Rust ownership/synchronization 的必要状态。

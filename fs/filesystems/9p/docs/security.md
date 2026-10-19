# v9fs — 安全与可靠性分析

## 信任模型

The analysis covers `src/` and the guest tests in `ut-tests/`. The extracted
metadata transport fixtures remain inside their original test modules under
`cfg(unittest)` and do not add a production transport or trust boundary.

服务端 metadata、qid、目录项、目录 offset、symlink target 和文件内容均为外部输入。v9fs
信任 KVFS 已完成 pathname、permission、mount namespace 和 user-memory boundary 检查；
PDU length、tag、type 和字段边界由 `p9` client 验证。

## 外部边界 / 攻击面

- mount 时的 negotiation、root qid 和 attributes；
- lookup/readdir 返回的 object identity、type、name 和 offset；
- read/write、setattr、statfs 和 namespace operation 的 server result；
- endpoint failure 与 synchronous fid Drop。

本 crate 不直接访问 user pointer、DMA/MMIO、firmware、interrupt、FFI 或 architecture raw
interface。transport 和 device safety 位于具体 provider/driver。

## unsafe 代码清单

本 crate 没有 `unsafe` block、unsafe API 或自定义 `Send`/`Sync` 实现。

## 内存安全不变量

- 每个 `V9fsInode` 持有 live、unopened fid，不保存 rename 后失效的 absolute path；
- 每个 opened `VfsFile` 的 typed private data 持有独立 opened fid；
- fid qid、attribute qid 与 VFS node type 一致后才发布 inode；
- inode identity 来自 server qid，不来自固定编号或 directory array index；
- namespace operation 只发送 KVFS 已解析的单个 child name；
- directory resume position 使用 server `d_off`；
- 自动时间更新只修改本地 inode，不额外发起远端 metadata 写入；
- 显式 setattr 失败时不发布修改，成功也不将服务端决定的 ctime 冒充客户端已确认值。

## 线程安全

v9fs 只共享 `Arc<P9Client>`/`Arc<P9Fid>`，不增加与协议状态分离的 session mutex。client
内部负责 request synchronization；inode、file 和 superblock 各自持有资源生命周期引用。
最后一个 fid reference 的 Drop 可阻塞，不得在 IRQ context 或持有 spinlock 时执行。

## 威胁分析

| 威胁 | 触发条件 | 影响 | 缓解 |
| --- | --- | --- | --- |
| stale path identity | rename 后按缓存 path 重新 walk | opened file 指向错误对象 | 只持 fid，不缓存 absolute path |
| type/qid spoofing | attributes 与 lookup fid 不一致 | 发布错误 inode 类型或身份 | qid path/type 与 mode type 交叉验证 |
| directory offset manipulation | server 返回异常 `d_off` | 重复或遗漏 entry | 原样使用 server offset，不伪造 local index |
| fid lifetime error | remove 后重复使用或 clunk | server handle misuse | `P9Fid` live state 与 `Arc` Drop |
| broad fallback | server 返回真实 I/O/permission error | 错误被掩盖 | 只对 Linux 指定的 `ENOSYS`/`EOPNOTSUPP` fallback |

## 故障模式与影响分析（FMEA）

| 故障模式 | 局部影响 | 系统影响 | 处理 |
| --- | --- | --- | --- |
| negotiation/attach/root getattr 失败 | mount 失败 | superblock 不发布 | error propagation 和 RAII cleanup |
| lookup 不存在 | negative lookup | 单个 pathname 不存在 | 只把 `ENOENT` 转成 negative result |
| create 成功、child walk 失败 | local inode 不发布 | remote object 可能已存在 | 返回 error，不伪造 local state |
| statfs 不实现 | 无 remote statistics | mount 仍可用 | 仅 `ENOSYS` 回退 `simple_statfs` |
| final clunk 失败 | remote fid 暂时滞留 | 到 connection teardown 才回收 | `p9` warning |
| endpoint 永久不响应 | caller task 阻塞 | 同 client operations 排队 | 当前无 timeout/cancel |

## 故障管理

`p9` 的 typed `KError` 直接传播为 VFS error。namespace/I/O failure 不使用 path-based retry；
unlink/rename 只按 Linux dotl semantics 对 `EOPNOTSUPP` 使用 protocol fallback，statfs 只对
`ENOSYS` 回退。

## 隐私分析

文件名、metadata、symlink target 和文件内容会暴露给选定的 9P endpoint。v9fs 不提供额外
加密，信任关系由启动配置、transport 和 VMM/host isolation 决定。

## 已知限制

- 只实现 9P2000.L cacheless semantics；
- KVFS 没有 dentry-private fid slot，lookup fid 因而放在 `V9fsInode`；
- 尚无 atomic-open contract、page cache、xattr 或 remote lock 支持；
- synchronous operations 没有 timeout/cancel。

## 审计清单

- VFS operation 是否使用 existing inode/file fid，而不是 absolute path；
- opened file 是否获得 independent fid 并放入 typed private data；
- namespace operation 是否保持 Linux fid role、fallback condition 和 wire order；
- new inode 是否在 qid/attribute validation 后才发布；
- 新字段是否对应 Linux semantics 或 KVFS/Rust ownership 的必要边界。

# p9 — 设计文档

## 定位

`net/9p` 对应 Linux `net/9p`，拥有 transport-independent 的 9P2000.L client、fid 和
PDU 编解码。它不实现 VFS 文件系统，也不负责设备发现；具体 transport 连接由调用者建立后
交给 `P9Client`。

| X-Kernel | Linux | 职责 |
| --- | --- | --- |
| `client.rs` | `net/9p/client.c`、`client.h` | `P9Client`、`P9Fid` 和协议操作 |
| `message.rs`、`protocol.rs` | `net/9p/protocol.c`、`9p.h` | PDU 编解码、qid 和协议常量 |
| `transport.rs` | `net/9p/transport.h` 的 connection 操作 | 一条已连接 endpoint 的 request 接口 |

`fs/filesystems/9p` 对应 Linux `fs/9p`，只把这些协议对象适配到 KVFS。

## 背景

数值 fid 只是 server handle，不足以表达 client ownership、qid、open 状态和最终 clunk。
本模块把 Linux 的 client/fid 引用关系直接编码为 Rust 对象，使 filesystem 层不再维护路径
字符串或裸 fid 生命周期。

## 范围

上表列出的 client、wire protocol 和 connection trait 属于本模块；transport discovery、具体
VirtIO driver 和 KVFS adapter 不属于本模块。

## 架构

```text
Arc<P9Fid> ──owns──► Arc<P9Client>
    │                    │
    │ object methods     └── Mutex<P9ClientState>
    │                         ├── msize / dialect
    │                         ├── tag / live-fid allocator
    │                         └── Box<dyn P9Transport>
    └── fid / qid / open mode / iounit / live state
```

`P9Fid` 是 Linux `struct p9_fid` 的 Rust 对象表达。`Arc` 直接承担 fid 引用计数，fid
保存 client 引用、服务端编号、qid、open mode、iounit 和是否仍有效。Linux 当前语义未使用
且不能影响对象行为的字段不复制到 Rust 结构。

`P9Client` 对应 Linux `struct p9_client` 的协议所有权。内部 sleepable mutex 串行化一条同步
transport connection 的请求以及 tag/fid 分配，上层无需再创建 session 级外锁。

## 状态机

```text
attach / complete walk
          │
          v
      unopened ──TLOPEN──► opened
          │                   │
          ├──TREMOVE──► consumed
          └──── final Arc drop ┴──► TCLUNK ──► released
```

- zero-name `walk()` 为同一远端对象建立独立 fid；非空 walk 只接受单个协议 name component；
- partial walk 不发布新对象，已经由服务端建立的 fid 会被 clunk；
- open 在 client mutex 内检查 unopened、发送请求并提交结果；失败保持 unopened，不增加 opening 状态；
- `TREMOVE` 提交后 fid 按 Linux 语义失效，即使请求返回错误也不再发送第二次 clunk；
- 最后一个 `Arc<P9Fid>` 释放时同步发送 `TCLUNK`，并在完成前保持 client/transport 存活。

## 算法流程

`P9Client::new()` 接管一条已连接 transport。`negotiate()` 只协商当前 filesystem adapter
实际实现的 9P2000.L，记录双方较小的 msize，分配 root fid 并执行 attach。

fid 对象直接提供 walk、open、getattr/setattr、read/write/readdir、fsync、statfs 和 namespace
操作。文件系统层不传绝对路径，也不能直接操作数值 fid。read、write 和 readdir 的单次数据量
同时受 negotiated maxdata 与服务端 iounit 限制。

每次同步 request 都验证：

- transport 返回长度不超过 response buffer；
- PDU 声明长度与实际长度一致，且至少包含 7-byte header；
- response tag 和类型与当前请求一致；
- 所有整数、qid、字符串和内嵌 data length 均在包边界内；
- 9P2000.L `RLERROR` 的数值 errno 直接转换为 `KError`。

fid allocator 跟踪仍存活的编号，回卷时不会复用尚未 clunk/remove 的 server handle；这对应
Linux client 的 fid IDR，而不是额外的 filesystem 状态。

## 调用约束 / 执行上下文

协议调用和 live fid 的 Drop 都可能阻塞，只能在 scheduler 和 allocator 可用后的可睡眠 task
context 执行；不要求 current userspace process，但不得从 IRQ context 或持有 spinlock 的路径
释放最后一个 fid 引用。同步 client connection 不可重入，重入和并发请求由 client mutex
串行化。

## 并发模型

client mutex 同时保护 negotiated state、tag/fid allocator 和 mutable transport request。
qid 使用 mutex，open mode、iounit 和 live lifecycle 使用原子状态，使共享 fid 的检查与最后
释放保持一致。open 的状态检查、响应提交及 I/O 的 iounit 上限计算位于同一个 client
临界区，避免在等待 open 时使用旧的 I/O 上限。

## 设计决策

- 9P 是可承载于 VirtIO、TCP、fd 等 transport 的协议，因此 client/fid 位于 `net/9p`；
- `P9Transport` 只表示一条 connection，不在本次改动中建立 transport registry 或搬迁驱动；
- transport 选择和 VirtIO device claim 仍属于现有 boot/driver glue；
- API 按 client/fid 对象表达，不保留 path-based convenience API；
- 当前只支持同步 9P2000.L，不声明尚未实现的 legacy/dotu、cache、xattr 或锁语义。

## Drop / 资源释放

最后一个 `P9Fid` 引用同步发送 `TCLUNK` 并从 client 的 live-fid set 释放编号；已经提交
`TREMOVE` 的 consumed fid 不再 clunk。fid 在清理完成前持有 client，最后一个 client/fid
引用释放后才释放 transport connection。

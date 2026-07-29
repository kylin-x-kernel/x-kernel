# tipc-handle — 安全与可靠性分析

## 信任模型

```text
tipc::syscall / process runtime / TIPC core
        │
        │ Arc<dyn Handle>, process-local handle ids
        ▼
┌────────────────────────────────────────────┐
│ tipc-handle                                │
│                                            │
│  Handle trait                              │
│  HandleSet registration                    │
│  HandleTable id ownership                  │
└────────────────────────────────────────────┘
        │
        ▼
kpoll / kspin / alloc
```

- handle id 来自用户态或上层调用者时不可信，只能在调用方持有的 `HandleTable` 中解析。
- `HandleTable` 信任 `Arc<dyn Handle>` 的实现维护具体对象生命周期和 close 语义。
- `HandleSet` 信任调用方传入的 `handle_id` 与 `Arc<dyn Handle>` 属于同一个 process-local table。
- cookie 是调用方 opaque 数据，本 crate 只保存和返回，不解释其含义。

## 外部边界 / 攻击面

| 边界 | 来源 | 进入形式 | 约束 |
|------|------|----------|------|
| handle id | syscall adapter 或内核调用者 | `i32` | 只能在当前 `HandleTable` 内解析 |
| handle object | TIPC core | `Arc<dyn Handle>` | 必须实现正确的 `poll`、`register`、`close` 和 `is_sendable` |
| handle set command | syscall adapter 或内核调用者 | `HandleSetCommand` | add/modify/delete 路径检查存在性和重复注册 |
| event mask | wait/poll 调用方 | `HandleEventMask` | 仅影响调用方观察的事件子集 |
| cookie | wait/poll 调用方 | `usize` | opaque，按值保存和返回 |

本 crate 不直接访问用户指针、地址空间、MMIO、DMA、FFI 或 inline assembly。

## unsafe 代码清单

`tee/tipc-handle/src` 当前没有 `unsafe` 块、`unsafe fn` 或 `unsafe impl`。

持续审计要求：

- 不使用裸指针保存 handle 所有权；
- 不使用 unchecked downcast；
- 不把 handle id 当作全局 capability；
- 不在 `HandleSet` 中允许未经循环检测的嵌套。

## 内存安全不变量

1. **handle 对象由 `Arc` 强引用持有**：`HandleTable` 和 `HandleSetEntry` 都保存 `Arc<dyn Handle>`，不会保存悬垂引用。
2. **downcast 只用于类型判断**：`HandleTable` 通过 `as_any().is::<HandleSet>()` 和 `downcast_ref::<HandleSet>()` 识别 handle set，不做 unchecked cast。
3. **handle set registration 清理先于 close**：删除普通 handle 时，先从所有 handle set 中移除 id，再调用目标 `close`。
4. **handle set 删除自身 id**：删除 handle set 时同时从 `handle_set_ids` 移除，后续 detach 不再访问已移除 handle set。
5. **禁止 handle set 嵌套**：`HandleSet::handle_set_ctrl` 拒绝 `HandleKind::HandleSet`，避免递归 poll 和循环引用。

## 线程安全

| 状态 | 并发保护 | 风险控制 |
|------|----------|----------|
| handle table map | 调用方外部锁 | `HandleTable` 不自带锁，通常由 `RwLock` 包装 |
| handle set entries | `SpinNoIrq<BTreeMap<...>>` | add/delete/modify/poll 串行化 |
| handle waiters | `kpoll::PollSet` + caller-owned `PollRegistrations` | `HandleWaitState::notify` 唤醒注册者，owner drop 注销等待者 |
| handle cookie | `AtomicUsize` | Acquire/Release 读写 caller cookie |

`HandleSet` 锁内只做 map 更新和短路径 poll，不应在锁内调用可能阻塞的操作。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | 用户伪造 handle id 访问其它进程对象 | 高 | 把 id 当作全局 id 使用 | `HandleTable` 是 process-local，由调用方选择当前进程表 |
| T-02 | handle set 持有已关闭 handle id | 中 | close 时未清理 registration | `uctx_handle_remove` 先 `detach_from_handle_sets` |
| T-03 | handle set 嵌套导致递归或循环 poll | 高 | 注册另一个 `HandleSet` | `handle_set_ctrl` 拒绝 `HandleKind::HandleSet` |
| T-04 | handle id 空间耗尽 | 中 | 进程持续打开 handle | 完整环形扫描后返回 `TooManyOpenFiles` |
| T-05 | 错误的 `Arc<dyn Handle>` 与 id 不属于同一 table | 中 | 调用方构造不一致 registration | syscall adapter 应从同一 `HandleTable` 取 handle 后注册 |
| T-06 | exec 后新映像继承旧 TIPC capability | 高 | exec 仅清理普通 `FD_CLOEXEC`，未关闭 process-local TIPC table | `Process::apply_exec_update` 调用 `uctx_handle_close_all`，显式关闭全部 TIPC handle |

影响等级定义：

- 高：可能造成跨进程 capability 暴露或 wait/poll 语义失控。
- 中：可能导致单进程 TIPC handle 操作失败、资源泄漏或错误事件。
- 低：短暂事件延迟或可重试错误。

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | `uctx_handle_get` 返回 `BadFileDescriptor` | id 不存在或已关闭 | 单次操作失败 | 调用方需处理无效 handle | 3 | map lookup typed error |
| F-02 | `uctx_handle_install` 返回 `TooManyOpenFiles` | id 空间完整扫描无空位 | 新 handle 无法安装 | 进程 TIPC 能力耗尽 | 2 | 环形扫描并明确返回错误 |
| F-03 | handle set add 返回 `AlreadyExists` | 重复注册同一 id | registration 失败 | 调用方需 modify 或 delete 后重试 | 3 | BTreeMap key 检查 |
| F-04 | handle set poll 返回 `NotFound` | handle set 为空 | wait 失败 | 调用方需先注册 handle | 4 | 明确空集合错误 |
| F-05 | delete/modify 返回 `NotFound` | 目标 registration 不存在或 cookie 不匹配 | 操作失败 | 调用方状态需同步 | 3 | 存在性和 cookie 检查 |
| F-06 | exec 后残留 port 或 channel | exec 未清理独立 TIPC table | 新程序可使用旧 capability | 可能跨 credential transition 保留 IPC 权限 | 2 | exec 时关闭全部 TIPC handle |

严重度定义：

- 1：致命，可能导致内存破坏或权限提升。
- 2：严重，导致进程 TIPC handle table 不可继续分配。
- 3：一般，单个 handle 或 handle set 操作失败。
- 4：轻微，可由调用方重新注册或重试恢复。

## 故障管理

- 所有外部可见失败使用 `KResult` 返回 typed error。
- id lookup 失败返回 `BadFileDescriptor`。
- handle set registration 不存在返回 `NotFound`。
- 资源上限返回 `TooManyOpenFiles`。
- close 路径唤醒等待者，使调用方能观察后续状态变化。

## 隐私分析

本 crate 保存 process-local handle id、event mask、cookie 和 `Arc<dyn Handle>`。
cookie 是调用方 opaque 数据，可能携带用户协议含义。
本 crate 不输出除调用方已注册 cookie 和 handle id 之外的新信息。

## 已知限制

- `HandleTable` 不内置锁，错误的外部并发使用会破坏逻辑一致性。
- `HandleSet` 禁止嵌套，未实现 Trusty 的非循环嵌套语义。
- `HandleSet` 信任调用方传入的 `handle_id` 与 `Arc<dyn Handle>` 对应同一 table。

## 审计清单

- [ ] 新增 handle 类型是否正确实现 `poll`、`register`、`close`、`cookie` 和 `is_sendable`？
- [ ] 新增 handle table 操作是否保持 process-local id 语义？
- [ ] 修改 close/remove 路径时是否仍先 detach handle set registration？
- [ ] exec 路径是否调用 `uctx_handle_close_all`，而不是仅清空 table map？
- [ ] 修改 handle set 嵌套策略时是否补充循环检测和测试？
- [ ] 新增 unsafe 时是否更新 unsafe 清单和 `SAFETY:` 注释？

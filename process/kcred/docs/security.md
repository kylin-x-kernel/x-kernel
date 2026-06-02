# kcred - 安全与可靠性分析

## 概述

`kcred` 不包含 Rust `unsafe` 代码。
它的主要风险不是裸指针或内存别名，
而是 POSIX 凭据状态转换错误导致越权、权限丢失后仍可恢复特权、
或权限检查使用了不一致的 UID/GID 快照。

本分析把 set-ID 转换、补充组排序和访问快照视为安全边界。

## 信任模型

```text
syscall layer / process resources
   │
   │ safe API:
   │   Credentials::{set_uid,set_gid,set_reuid,set_regid}
   │   Credentials::{set_resuid,set_resgid,set_fsuid,set_fsgid}
   │   Credentials::access_snapshot
   v
┌─────────────────────────────────────────────┐
│ kcred                                      │
│                                             │
│ policy boundary                            │
│  ├─ euid == 0 privileged model             │
│  ├─ real/effective/saved ID transition     │
│  ├─ fsuid/fsgid tracking                   │
│  └─ sorted supplementary group invariant   │
│                                             │
│ unsafe boundary: none                      │
└──────────────────────┬──────────────────────┘
                       │ immutable snapshots
                       v
VFS / DAC permission checks
```

- safe API 调用者信任 `kcred` 正确维护 POSIX ID 转换规则。
- `kcred` 信任 syscall 层已经把用户参数解析为 `Uid`、`Gid` 或 `None`。
- `kcred` 信任调用者在修改进程凭据时持有合适的进程级写锁。
- 权限检查者应使用 `AccessCredentials` 快照，
  不应在检查过程中观察可变 `Credentials`。

## unsafe 代码清单

本 crate 没有 `unsafe` 块、`unsafe fn`、`unsafe impl` 或裸指针操作。

## 内存安全不变量

1. **补充组快照不可变**：
   `supplementary_groups` 存储为 `Arc<[Gid]>`，
   写入时整体替换，快照持有期间不会被原地修改。
2. **补充组保持排序**：
   只有 `set_supplementary_groups` 能替换补充组，
   且写入前执行 `sort_unstable()`。
   `AccessCredentials::has_group` 依赖该排序执行 `binary_search`。
3. **凭据字段只通过 `&mut Credentials` 修改**：
   crate 内部没有 interior mutability，
   Rust 借用规则防止同一 `Credentials` 被同时可变访问。
4. **访问快照不借用可变凭据**：
   `access_snapshot` 复制 UID/GID 并克隆补充组 `Arc`，
   权限检查不依赖原始 `Credentials` 的生命周期。

## 权限语义不变量

1. **非特权 set-ID 目标受限**：
   非特权调用只能把目标 UID/GID 设置为当前 real、effective 或 saved ID。
2. **filesystem ID 跟随 set-ID 转换**：
   `set_uid`、`set_gid`、`set_resuid`、`set_resgid`
   以及有效 ID 变化的 `set_reuid`、`set_regid`
   必须同步更新 `fsuid` 或 `fsgid`。
3. **`setfsuid` / `setfsgid` 不报错但保持旧值**：
   被拒绝的请求必须不改变状态，
   并返回旧 filesystem ID。
4. **`execve` 后 saved ID 跟随 effective ID**：
   `apply_exec` 必须在 setuid/setgid 可执行文件逻辑更新 effective ID 后调用。
5. **当前特权模型只等价于 `euid == 0`**：
   新增 capability 支持时不得继续把所有 root 权限隐式折叠到该判断。

## 线程安全

| 类型 | Send 条件 | Sync 条件 |
|------|-----------|-----------|
| `Credentials` | 字段均满足 `Send` | 共享读取安全；修改需要上层锁提供独占访问 |
| `AccessCredentials` | `Uid`、`Gid` 和 `Arc<[Gid]>` 满足 `Send` | 不可变快照，可共享读取 |
| `CredentialError` | plain enum | plain enum |

`kcred` 不提供全局凭据存储。
线程安全由 `kprocess` / `kresources` 等进程状态容器的锁负责。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | 非特权进程把 UID/GID 改成任意值 | 高 | set-ID 校验遗漏 real/effective/saved 集合限制 | `check_uid_change` / `check_gid_change` 统一校验；单元测试覆盖拒绝路径 |
| T-02 | effective ID 变化后 filesystem ID 未同步 | 高 | set-ID 转换只改 `euid`/`egid` | `set_uid`、`set_gid`、`set_resuid`、`set_resgid` 和相关 `set_reuid`/`set_regid` 路径同步更新 fs ID |
| T-03 | `setfsuid` 或 `setfsgid` 被拒绝后仍改变状态 | 高 | 返回旧值语义和状态更新混淆 | 函数先保存旧值，只有允许目标时才写入 |
| T-04 | 补充组未排序导致 `has_group` 漏判 | 高 | 调用者绕过 `set_supplementary_groups` 写入无序数组 | 字段私有；唯一写入口排序后整体替换 |
| T-05 | 权限检查期间凭据被并发修改产生 TOCTOU | 中 | VFS 直接引用可变 `Credentials` 执行多步检查 | 提供 `AccessCredentials` 快照；调用方应在检查前复制所需 ID |
| T-06 | 内部 unchecked helper 被外部误用绕过权限策略 | 高 | helper 作为 `pub` API 暴露给上层 syscall 路径 | 已将 `set_resuid_unchecked` / `set_resgid_unchecked` 收窄为 `pub(crate)`；新增 syscall 入口应调用 checked API |
| T-07 | `apply_exec` 调用顺序错误丢失 setuid/setgid 程序语义 | 中 | setuid executable 支持先重置 saved ID 后更新 effective ID | `design.md` 记录顺序要求；未来 exec loader 改动需联动审计 |
| T-08 | root 简化模型与 capability 语义不一致 | 中 | 引入 capability 后仍只检查 `euid == 0` | 把 `is_privileged` 作为替换点；capability 支持应在策略层接入 |

影响等级定义：

- 高：导致 UB、内存破坏、权限提升。
- 中：导致 panic、服务不可用、数据不一致。
- 低：导致性能退化、日志丢失、功能降级。

## 故障模式与影响分析

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | `set_uid` / `set_gid` 返回 `PermissionDenied` | 非特权目标不在允许集合 | syscall 映射为权限错误 | 调用进程 ID 不变 | 4 | 返回显式错误，状态不变 |
| F-02 | `set_resuid` / `set_resgid` no-op | 目标均为 `None` 或当前值 | 不写入字段 | 无影响 | 4 | `is_resuid_noop` / `is_resgid_noop` 快速返回 |
| F-03 | 补充组数组分配失败 | `Vec` 或 `Arc<[Gid]>` 分配失败 | 当前 syscall 可能 panic 或上层分配阶段失败 | 内存压力下服务不可用 | 2 | 上层应在未来把分配失败映射为 `ENOMEM` |
| F-04 | `binary_search` 未命中已有组 | 补充组排序不变量被破坏 | DAC 检查错误拒绝访问 | 应用行为异常 | 2 | 字段私有，写入口排序；审计新增写入口 |
| F-05 | 凭据锁持有不足 | 调用方在无写锁上下文修改 `Credentials` | 逻辑竞态 | 权限检查结果不一致 | 2 | crate 只暴露 `&mut self` 修改；进程状态容器负责锁 |
| F-06 | exec 后 saved ID 未更新 | 调用方忘记 `apply_exec` | 程序无法按 POSIX 规则恢复/丢弃权限 | setuid 程序行为错误 | 3 | exec 路径应在凭据切换后固定调用 |

严重度定义：

- 1：致命，系统崩溃、数据丢失。
- 2：严重，功能不可用，需重启恢复。
- 3：一般，功能降级，可自动恢复。
- 4：轻微，影响有限，用户可容忍。

## 故障管理

- 凭据转换被拒绝时返回 `CredentialError::PermissionDenied`，
  syscall 层负责映射为 Linux errno。
- `setfsuid` 和 `setfsgid` 遵循 Linux 语义，
  不通过错误码报告拒绝，而是返回旧 ID 并保持状态不变。
- 本 crate 不记录日志，
  避免在权限检查热路径暴露用户或进程身份信息。
- 本 crate 没有 panic 恢复机制；
  可能的分配失败风险来自补充组替换时的上层 `Vec` / `Arc` 构造。

## 隐私分析

`kcred` 保存用户 ID、组 ID 和补充组集合。
这些值是进程安全身份的一部分，
但不包含用户名、路径、文件内容或网络载荷。
模块自身不持久化、不打印日志。
调用方在调试日志中输出凭据时应避免泄露跨进程身份关系。

## 已知限制

1. **无 capability 模型**：
   当前 `is_privileged` 使用 `euid == 0`。
2. **无 LSM / namespace 语义**：
   UID/GID 是全局数值，没有 user namespace 映射。
3. **无补充组数量限制**：
   本 crate 不检查 `NGROUPS_MAX`；
   syscall 层应负责用户输入数量限制。
4. **setuid/setgid executable 支持尚未接入**：
   `apply_exec` 目前只重置 saved ID，
   未来接入文件 capability 或 setuid/setgid 位时需在调用前更新 effective ID。

## 其它说明（模板章节）

| 章节 | 说明 |
|------|------|
| 基线 | 以本仓库 `docs/templates/module-docs-guide.md` 及 `AGENTS.md` 为准 |
| 冗余设计 | 无 |
| 过载控制 | 无 |
| 人因差错 | 无直接用户交互 |
| 故障预测预防 | 无 |
| 升级不中断业务 | 无 |

## 审计清单

修改 `kcred` 时需验证：

- [ ] 新增凭据字段在所有 set-ID 转换中保持一致。
- [ ] 非特权 UID/GID 变更仍限制在 real/effective/saved 集合内。
- [ ] `fsuid` / `fsgid` 跟随 effective ID 的路径没有遗漏。
- [ ] 新增补充组写入口保持排序不变量。
- [ ] 访问检查新增入口优先使用 `AccessCredentials` 快照。
- [ ] capability、namespace 或 LSM 接入时替换 `is_privileged` 的策略边界。
- [ ] 若新增 `unsafe` 代码，逐项补充 `SAFETY:` 注释和本文件 unsafe 清单。

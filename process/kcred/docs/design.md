# kcred - 设计文档

## 定位

`kcred` 是 x-kernel 的 POSIX 进程凭据模型 crate。
它保存进程的真实、有效、保存和文件系统用户/组 ID，
并提供 Linux `setuid`、`setgid`、`setreuid`、`setregid`、
`setresuid`、`setresgid`、`setfsuid`、`setfsgid` 和 `execve`
后的凭据转换语义。

目标读者是维护 syscall 凭据路径、文件权限检查路径和进程资源复制路径的开发者。

## 背景

内核需要把用户态传入的 UID/GID 变更请求转化为可审计的状态转换，
并为 VFS、DAC 权限检查和后续 capability 支持提供稳定的凭据快照。
`kcred` 把这些规则从 syscall 实现和 VFS 中分离出来，
使调用者只负责参数解析、错误码映射和持锁，
凭据 crate 负责维护 ID 之间的不变量。

当前实现采用简化的特权模型：
有效 UID 为 0 的进程被视为拥有 set-ID 操作所需权限。
Linux capability 的细粒度权限尚未引入。

## 范围

涉及的源文件：

```text
process/kcred/
├── src/
│   ├── lib.rs                  # crate 入口和公开 re-export
│   ├── credentials/
│   │   ├── mod.rs              # credentials 子模块组织
│   │   ├── model.rs            # Credentials 状态与 set-ID 转换
│   │   ├── access.rs           # AccessCredentials 与访问检查快照
│   │   ├── error.rs            # CredentialError
│   │   ├── user.rs             # Uid 类型
│   │   └── group.rs            # Gid 类型
│   └── tests.rs                # set-ID 与访问快照单元测试
├── Cargo.toml
└── docs/
    ├── design.md
    └── security.md
```

## 架构

```text
syscall / process runtime
        │
        │ mutates Credentials under caller-owned process lock
        v
┌─────────────────────────────────────────────┐
│ kcred::Credentials                           │
│  ruid/euid/suid/fsuid                        │
│  rgid/egid/sgid/fsgid                        │
│  supplementary_groups: Arc<[Gid]>            │
└──────────────┬──────────────────────────────┘
               │ access_snapshot(kind)
               v
┌─────────────────────────────────────────────┐
│ kcred::AccessCredentials                     │
│  uid/gid + sorted supplementary groups        │
│  has_group(gid) via binary_search             │
└─────────────────────────────────────────────┘
               │
               v
VFS / DAC permission checks
```

| 组件 | 职责 |
|------|------|
| `Credentials` | 保存 POSIX 凭据状态并实现 set-ID 状态转换 |
| `AccessCredentials` | 为权限检查提供不可变快照，避免检查期间读到半更新状态 |
| `AccessIdKind` | 指定权限检查使用真实、有效还是文件系统 ID |
| `CredentialError` | 表示凭据转换被权限规则拒绝 |
| `Uid` / `Gid` | Linux 用户和组 ID 的本地类型别名 |

## 状态机

### 用户 ID 转换

```text
Unprivileged
   │ set_uid(uid in {ruid,euid,suid})
   │ set_reuid / set_resuid with allowed IDs
   v
Unprivileged

Privileged(euid == 0)
   │ set_uid / set_resuid(any uid)
   v
MaybeUnprivileged(new euid may be non-zero)

Exec
   │ apply_exec()
   v
suid = euid, sgid = egid
```

| 从 | 到 | 触发条件 |
|----|----|----------|
| Unprivileged | Unprivileged | 目标 UID/GID 属于当前 real/effective/saved 集合 |
| Unprivileged | Rejected | 目标 UID/GID 不在允许集合，返回 `PermissionDenied` |
| Privileged | MaybeUnprivileged | `euid == 0` 时允许写入任意 real/effective/saved ID |
| Any | ExecApplied | `apply_exec()` 将 saved ID 重置为当前 effective ID |

### 文件系统 ID 转换

```text
fsuid/fsgid current
   │ setfsuid/setfsgid(allowed target)
   v
fsuid/fsgid updated

fsuid/fsgid current
   │ setfsuid/setfsgid(disallowed target)
   v
unchanged, return old fsuid/fsgid
```

`setfsuid` 和 `setfsgid` 总是返回旧值。
如果请求不被允许，状态保持不变，
由 syscall 层按 Linux 语义把返回值交给用户态。

## 算法流程

### `set_uid` / `set_gid`

1. 若当前 `euid == 0`，直接把 real、effective、saved 和 filesystem ID 都更新为目标值。
2. 否则校验目标值必须等于当前 real、effective 或 saved ID。
3. 非特权调用只更新 effective 和 filesystem ID。
4. 校验失败返回 `CredentialError::PermissionDenied`。

### `set_reuid` / `set_regid`

1. 保存旧 real/effective ID。
2. 非特权调用分别校验 real 与 effective 目标是否在允许集合中。
3. 应用非 `None` 参数；`None` 表示 syscall 参数 `-1`。
4. 当 real ID 被修改，或 effective ID 被修改为不同于旧 real ID 的值时，saved ID 跟随新的 effective ID。
5. 任一 real/effective 参数被修改时，filesystem ID 跟随新的 effective ID。

### `set_resuid` / `set_resgid`

1. 非特权调用要求每个非 `None` 目标都等于当前 real、effective 或 saved ID。
2. 若请求是 no-op，直接返回，避免无意义地重写 filesystem ID。
3. 应用 real、effective、saved 三个可选字段。
4. filesystem ID 跟随最终 effective ID。

### 补充组

1. `set_supplementary_groups` 接收调用者传入的 `Vec<Gid>`。
2. 在写入前执行 `sort_unstable()`。
3. 以 `Arc<[Gid]>` 保存，快照可以无复制地被访问检查持有。
4. `AccessCredentials::has_group` 使用 `binary_search`，
   因此补充组有序是核心不变量。

## 并发模型

`kcred` 本身不包含锁和原子操作。
`Credentials` 的修改需要调用者在进程状态或资源锁下串行化。
`AccessCredentials` 是不可变快照，
内部使用 `Arc<[Gid]>` 共享补充组数组，
可在权限检查期间跨引用持有。

并发约束：

- 修改 `Credentials` 时必须由拥有该进程凭据写权限的上层持锁。
- 权限检查不应直接持有可变 `Credentials` 引用，
  应使用 `access_snapshot` 获得稳定快照。
- `supplementary_groups` 只能通过 `set_supplementary_groups` 写入，
  以保留排序不变量。

## Drop / 资源释放

`Credentials` 和 `AccessCredentials` 没有自定义 `Drop`。
补充组数组由 `Arc<[Gid]>` 管理引用计数。
当进程或快照释放后，补充组存储随最后一个 `Arc` 自动释放。

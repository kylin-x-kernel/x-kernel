# kcred - 安全与可靠性分析

## 概述

`kcred` 没有 Rust `unsafe` 代码。主要风险来自凭据转换规则错误、已提交对象被原地
修改、一次权限操作混用多个身份快照、securebits 锁定位处理错误，以及补充组不变量
被破坏。

## 信任边界

```text
untrusted syscall IDs
        |
        v
kprocess: prepare Cred -> checked transition -> commit Arc<Cred>
        |                                      |
        | stable snapshot                      +--> task identity queries
        v
kvfs: explicit &Cred -> generic DAC / filesystem callbacks
```

- syscall 层负责把 ABI 参数转换为 `Uid`、`Gid` 或 `None`，并限制补充组数量。
- `prctl(PR_SET_KEEPCAPS)` 的 ABI 参数校验由 syscall 层完成，锁定位和 securebits 状态
  转换由 `kcred` 完成。
- `kcred` 负责 set-ID 规则和凭据内部不变量。
- `kprocess` 负责当前任务定位、发布和替换 committed credential。
- `kvfs` 负责基于 `fsuid/fsgid`、补充组和 inode 元数据执行 DAC。

## unsafe 代码清单

本 crate 没有 `unsafe` 块、`unsafe fn`、`unsafe impl` 或裸指针操作。

## 安全不变量

1. **已提交对象不可变**：公开写操作只接受 `&mut Cred`；调用者只能修改未提交副本。
2. **原子发布**：字段转换全部成功后才由 `kprocess` 替换 `Arc<Cred>`。
3. **快照一致**：一次 namei、exec 或 access 操作使用入口取得的同一个快照。
4. **补充组有序**：唯一替换入口排序后整体替换 `Arc<[Gid]>`，`in_group()` 才可安全
   使用二分查找。
5. **失败不发布**：checked set-ID 返回 `KError::OperationNotPermitted` 时，当前线程仍
   指向旧 committed credential。
6. **filesystem IDs 同步**：需要改变 effective ID 的转换按 Linux 规则同步 fs ID；
   明确的 `setfsuid/setfsgid` 除外。
7. **access 不修改当前身份**：默认 access 只通过 `for_access()` 修改临时副本；
   `AT_EACCESS` 直接借用当前不可变快照并保留显式设置的 `fsuid/fsgid`。
8. **securebits 状态受锁定位保护**：`SECBIT_KEEP_CAPS_LOCKED` 置位后，
   `keep_caps_enable()` / `keep_caps_disable()` 返回 `OperationNotPermitted`，失败的
   prepared credential 不会发布。
9. **exec 清除 keep-capabilities**：`apply_exec()` 在提交 exec 凭据前清除
   `SECBIT_KEEP_CAPS`。
10. **real-credential 匹配非对称**：跨任务 real-credential 检查只使用调用者的 real
    UID/GID，并要求它们分别匹配目标的 real、effective 和 saved UID/GID；不能逐字段
    比较两份完整凭据。

## 线程安全

| 对象 | 并发语义 |
|------|----------|
| `Arc<Cred>` | 不可变 committed snapshot，可跨线程共享 |
| prepared `Cred` | 发布前由单个调用者独占修改 |
| `Arc<[Gid]>` | 不可变且有序，可被多个凭据副本共享 |
| `initial_cred()` | 由 `Once` 初始化并返回共享 `Arc` |

`kcred` 不提供全局“当前凭据”。线程锁与 current-task 访问由 `kprocess` 提供，避免把
调度上下文引入文件系统底层。

## 威胁分析

| 编号 | 威胁描述 | 影响 | 应对措施 |
|------|----------|------|----------|
| T-01 | 非特权任务切换到任意 UID/GID | 权限提升 | checked set-ID API 限制目标集合并返回 `OperationNotPermitted` |
| T-02 | 转换中途被权限检查观察 | 身份混合、越权 | prepare/commit 与不可变 `Arc<Cred>` 原子发布 |
| T-03 | 一个路径操作中途读取新 current cred | 分段使用不同身份 | syscall 入口只 snapshot 一次并显式逐层传递 |
| T-04 | 补充组无序导致漏判或误判 | DAC 结果错误 | 字段私有，替换入口排序，单元测试覆盖 |
| T-05 | `access(2)` 错用 fs/effective IDs | 探测结果错误 | 默认检查使用 `for_access()` 映射到 real IDs；`AT_EACCESS` 不 override 当前 filesystem IDs |
| T-06 | root 近似被误认为完整 capability 模型 | 权限边界过宽 | 文档明确限制；后续在策略边界接入 capability |
| T-07 | exec 忘记固定 saved IDs | 特权恢复语义错误 | exec credential 副本调用 `apply_exec()` 后再提交 |
| T-08 | 初始 root credential 被原地修改 | 全局权限破坏 | `initial_cred()` 只发布 `Arc<Cred>`，变更必须 prepare 新对象 |
| T-09 | 锁定的 keep-capabilities 状态被修改 | 后续 exec 或 UID 转换的权限边界失效 | `keep_caps_enable()` / `keep_caps_disable()` 先检查 `SECBIT_KEEP_CAPS_LOCKED`，失败路径不提交凭据 |
| T-10 | real-credential 检查逐字段比较 caller/target | 错误放行 set-ID 目标或拒绝合法调用者 | `matches_real_credential_ids()` 固定使用 caller real UID/GID 与 target real/effective/saved IDs 比较，非对称测试覆盖两类反例 |

## 故障模式与处理

| 故障 | 局部结果 | 处理 |
|------|----------|------|
| checked set-ID 被拒绝 | prepared 副本不提交 | 传播 `KError::OperationNotPermitted` |
| `setfsuid/setfsgid` 目标不允许 | 字段不变 | 返回旧 ID，遵循 Linux ABI |
| `PR_SET_KEEPCAPS` 参数非法或 securebits 已锁定 | prepared 副本不提交 | 分别返回 `InvalidInput` 或 `OperationNotPermitted` |
| 补充组分配失败 | 内存压力下操作不能完成 | 当前分配器策略生效；未来可在可失败分配边界映射 `ENOMEM` |
| override 状态下普通 commit | objective/subjective 语义可能被覆盖 | `kprocess` 当前以断言拒绝该未支持状态 |

本 crate 不记录凭据日志，避免在权限热路径泄露跨任务身份关系。

## 已知限制

1. 无完整 capability 集合、LSM 或 file capability；securebits 仅覆盖
   `KEEP_CAPS` 及其锁定位。
2. 无 user namespace ID 映射和 idmapped mount DAC。
3. 无临时 subjective credential override。
4. setuid/setgid executable 尚未接入。
5. 补充组分配仍依赖全局不可恢复分配策略。

## 审计清单

- 新转换是否只修改 prepared `Cred`，并在成功后一次提交。
- UID/GID 检查是否使用正确的 real/effective/saved 集合。
- real-credential 匹配是否保持 caller-real 对 target 三类 ID 的非对称语义。
- keep-capabilities 锁定位是否阻止后续修改，exec 是否清除 `KEEP_CAPS`。
- `fsuid/fsgid` 与补充组是否保持 DAC 所需不变量。
- 多步检查是否复用同一个 `Arc<Cred>`。
- 是否避免新增 current-task 全局依赖或冗余 access 快照类型。
- 新增 `unsafe` 时是否逐项记录安全前提和 `SAFETY:` 注释。

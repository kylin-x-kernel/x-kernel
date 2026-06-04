# ksyscall - 安全与可靠性分析

## 概述

`ksyscall` 是用户态 syscall ABI 进入内核资源 owner 的第一层适配边界。
主要风险来自：

- 用户提供的 syscall number、标志位、fd、PID 和标量参数；
- 用户指针 `copyin/copyout`；
- 错误的 owner 路由导致权限或语义检查落错位置；
- 在 adapter 层误持有资源状态，造成边界混乱。

本 crate 当前不包含手写 `unsafe` 代码。

## 信任模型

```text
userspace syscall arguments
   │ untrusted
   v
ksyscall
   │ validates ABI shape and dispatches
   v
resource owners
   ├─ posix-fs / kfs / kvfs
   ├─ kfd_objects
   ├─ kthread
   ├─ posix-process / kprocess
   └─ other subsystem owners
```

- 用户态 syscall 参数不可信。
- `ksyscall` 信任各 owner crate 在进入其边界后维护真实资源语义。
- `ksyscall` 必须在 syscall 边界完成 ABI 级别的基础校验，
  但不应重复 owner 内部不变量检查。

## 核心不变量

1. `ksyscall` 不保存资源的长期状态。
2. 用户指针只通过现有安全封装类型访问。
3. syscall adapter 负责 ABI 级错误码分支，不越权实现 owner 逻辑。
4. adapter 目录结构应反映 owner 归属，而不是历史 API 分类。
5. 涉及 current process/thread 的 helper 只能在明确上下文下调用。

## 主要风险

| 编号 | 风险 | 影响 | 缓解 |
|------|------|------|------|
| T-01 | 用户坏指针导致 copyin/copyout 失败 | 中 | 统一通过 `UserPtr`/`UserConstPtr`/现有封装访问并传播 `KResult` |
| T-02 | adapter 在错误 owner 下落地，导致边界职责重新混乱 | 中 | 目录和文档按 owner 语义组织；review 时检查路由归属 |
| T-03 | adapter 重复实现 owner 状态机，造成双重语义源 | 高 | 文档明确 `ksyscall` 不拥有长期状态；仅做 ABI 适配 |
| T-04 | current-thread/process helper 在错误上下文调用 | 中 | 复用 `kthread` 现有约束，并在 syscall 入口保持 task-context 假设 |
| T-05 | 不同 syscall 被历史目录误导，后续继续堆入错误模块 | 中 | crate-local design 文档固定 `vfs/ipc/time/task` 的 adapter 语义 |

## 审计清单

- [ ] 新增 syscall 实现是否只做 ABI 适配，而不是复制 owner 状态机？
- [ ] 新增 adapter 是否放在贴近 owner 的目录，而不是历史 API 杂项目录？
- [ ] 用户指针访问是否都通过现有封装类型？
- [ ] current process/thread helper 的调用上下文是否明确？
- [ ] 如果修改了 owner 路由关系，是否同步更新本 crate 和 owner crate 文档？

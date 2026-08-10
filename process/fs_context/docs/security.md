# fs_context — 安全与可靠性分析

## 信任边界

本 crate 接收已经由 VFS 解析得到的 `Path`。用户 pathname、权限和 namespace 可见性
必须在调用前由 KVFS/POSIX 层校验；`fs_context::FsStruct` 只维护解析结果的共享状态。

## unsafe 与内存安全

`process/fs_context/src` 没有 `unsafe` block。

- boot 通过 `attach_root` 同时初始化 root 和 pwd；后续更新维持有效路径。
- root、pwd 的替换目标必须是目录。
- `Path` 通过引用计数维持 mount/dentry 生命周期。
- umask 只保留 `0o777` 范围内的位。
- 所有共享读写都通过外层 `Mutex`。

## 并发与故障模式

| 故障模式 | 结果 | 缓解 |
|---|---|---|
| boot 完成前读取 root/pwd | 内核 panic | 启动顺序必须先 `attach_root` 再创建用户进程 |
| chdir/chroot 目标非目录 | 状态不变 | 返回 `VfsError::NotADirectory` |
| mount namespace copy 后仍引用旧 mount tree | 路径跨 namespace | namespace clone 使用成对 root/pwd retarget |
| mount I/O 期间长期持有 fs_struct 锁 | 阻塞并发 chdir/chroot | 先取得 `root_and_pwd` 引用快照，再释放锁 |
| `CLONE_FS` 组合错误 | 非预期共享路径环境 | clone/namespace 层校验 flags 并选择共享或复制 |

## 隐私与已知限制

对象保存内核 `Path` 引用和 umask，不保存 pathname 文本或文件内容。early boot 的
`None` 只对应 Linux 静态 `init_fs` 中尚未安装的零值 `root/pwd`；用户进程创建前必须
已经通过 `attach_root` 安装有效路径。

## 审计清单

- 新 API 是否错误地把 `FsStruct` 称作 mount `FsContext`？
- 更新 root/pwd 前是否验证目录并保持成对状态？
- clone flags 是否正确选择共享或复制？
- 路径解析或设备 I/O 前是否只取得快照而没有长期持锁？

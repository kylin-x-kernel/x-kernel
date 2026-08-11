# POSIX 与 Linux 兼容性审查规则

本阶段检查 PR 中对用户空间可见的 syscall、ABI 和内核接口语义，
确认其与 X-Kernel 目标支持的 Linux 行为和相关 POSIX 约定一致。
开始分析前必须继续读取 `docs/ai/review/common.md`。

review 服务会在运行时提示中提供 Linux 源码路径。
该路径由 `read_file`、`search_content` 和 `search_files` 工具解析，
不要把某台机器上的绝对路径写入本规则。

## 阶段范围

重点覆盖：

- syscall 参数、返回值、errno 和副作用；
- 文件、目录、fd、pipe、socket 和设备文件语义；
- 进程、线程、信号、等待和凭据；
- mmap、munmap、mprotect、brk、共享内存和页错误可见行为；
- futex、IPC、poll/epoll 和阻塞/唤醒；
- 用户可见结构体布局、flag、常量和时间单位；
- procfs、sysfs、ioctl 等 Linux 特有兼容接口。

纯内部 API 不必强行套用 POSIX。
如果变更不涉及用户可见或 Linux 兼容语义，应尽快结束并输出 `未发现问题`。

## 权威来源与使用顺序

按以下优先级查证：

1. 对应 syscall 的 Linux man page；
2. POSIX / The Open Group 规范（适用于标准接口时）；
3. 当前配置的 Linux 内核源码；
4. X-Kernel 已有同类 syscall 和公共抽象；
5. 项目测试与兼容性约定。

man page 用于确认外部契约，Linux 源码用于理解边界条件和实现细节。
不能只看 Linux 实现中的单一分支就推导完整 ABI，
也不能只凭 man page 忽略 X-Kernel 已明确选择的兼容范围。

## 强制工作流

1. 使用 `get_annotated_diff` 获取变更。
2. 识别所有受影响的 syscall、用户可见结构、flag 和 errno 路径。
3. 使用 `fetch_man_page` 获取对应接口文档，阅读参数、返回值、errors 和 notes。
4. 使用 `search_content` / `search_files` 在 Linux 源码中找到实际入口及关键 helper。
5. 使用 `read_file` 阅读相关 Linux 实现，不只停留在搜索片段。
6. 阅读 X-Kernel 中相邻 syscall、用户内存 helper、fd 抽象和现有测试。
7. 建立“输入 — 状态 — 返回值/errno — 副作用”对照表。
8. 只报告能指出规范或 Linux 参考证据的语义差异。

## X-Kernel syscall 基本约束

审查当前代码库实际约定，并重点确认：

- syscall 函数使用 `sys_` 前缀并返回项目约定的 `KResult<isize>`；
- 用户地址通过 `UserPtr<T>`、`UserConstPtr<T>` 或当前项目等价抽象表示；
- 用户内存读写通过 `read_vm()`、`write_vm()`、`load_vm_vec()` 等受控接口；
- 可选指针在解引用前正确处理 null 语义；
- 用户提供的长度、flag、fd、枚举和地址在 syscall 边界验证；
- 内部错误准确映射到对用户可见的 errno；
- 成功返回值使用正确单位和 signedness。

如果代码库约定已经演进，应以源分支中的当前抽象为准，
不要为了匹配本文件中的旧名称而要求倒退。

## 重点检查项

### 1. 参数验证顺序

Linux syscall 的多个错误条件同时成立时，验证顺序可能影响 errno。
检查：

- flag 是否拒绝未知位；
- 长度、对齐、范围和结构体版本是否合法；
- fd 类型、权限和对象状态是否在正确阶段检查；
- null 指针在该接口中是错误、可选值还是特殊命令下被忽略；
- 即使后续参数无效，是否应先产生某个固定 errno；
- 用户内存访问发生在会产生其他优先错误的检查之前还是之后。

只有在用户可观察且有规范/测试依据时报告验证顺序差异。

### 2. 返回值与 errno

确认：

- 成功时返回 0、字节数、fd、地址或剩余时间是否正确；
- short read/write、部分完成和被信号中断的返回规则；
- `EINTR`、`EAGAIN`、`EINVAL`、`EFAULT`、`EBADF` 等是否在正确条件下产生；
- 内部 `NotFound`、权限或资源不足错误是否映射到正确 errno；
- restartable syscall 是否遵守项目已有 restart 机制；
- errno 不会在成功路径被意外覆盖。

评论中应写明预期 errno 和触发输入。

### 3. 副作用与原子性

检查失败或部分成功时：

- 文件 offset、时间戳、引用计数或对象状态是否已改变；
- 向用户缓冲区写入了多少数据；
- fd 是否已安装、关闭或可被其他线程观察；
- rename、link、mount、mmap 等多对象操作是否保持原子可见性；
- copy_to_user 失败后的副作用是否符合 Linux；
- 阻塞操作被信号打断后是否正确保留或回滚状态。

### 4. 文件与 fd 语义

重点检查：

- open flags、`O_NONBLOCK`、`O_CLOEXEC`、append 和 truncation；
- file description 与 fd table entry 的共享关系；
- dup/fork/exec 后 offset、flags 和 close-on-exec 行为；
- 目录、symlink、mount namespace 和路径解析边界；
- EOF、短 I/O、seekability 和 pipe 原子写入；
- poll readiness 是否与实际后续操作一致。

### 5. 进程、线程与信号

检查：

- PID/TID 选择和线程组语义；
- fork/clone/exec/exit/wait 的资源继承与回收；
- signal mask、pending、default action 和 restart；
- wait status 编码、zombie 生命周期和并发 wait；
- credentials、session、process group 和权限判断；
- futex key、共享/私有语义和超时单位。

### 6. 内存管理接口

检查：

- 地址和长度页对齐及 overflow；
- `MAP_FIXED`、shared/private、anonymous/file-backed 组合；
- protection 与文件权限；
- partial unmap、VMA split/merge 和错误原子性；
- COW、文件截断、msync、madvise 和 mlock 可见语义；
- brk 返回约定与资源限制；
- fault 时 signal / errno 行为。

涉及复杂 MM 语义时，可进一步读取
`docs/ai/skills/linux-mm-design-knowledge/` 中对应主题，
但最终 finding 仍需落到当前 PR 的可见行为差异。

### 7. ABI、结构体与时间

检查：

- `repr(C)`、字段顺序、padding、大小和对齐；
- 32/64 位架构差异、compat syscall 和指针宽度；
- endian、bitflag 数值和保留字段；
- `timespec`、tick、纳秒/微秒/毫秒换算；
- 用户结构复制时的未初始化 padding 或版本兼容；
- ioctl command 编码和结构大小。

## Linux 源码常用区域

- `kernel/`：进程、信号、调度相关核心逻辑；
- `fs/`：VFS、文件、目录、fd、poll 和 ioctl；
- `ipc/`：System V IPC 和共享机制；
- `net/`：socket 和网络协议接口；
- `mm/`：虚拟内存、mmap、fault 和回收；
- `include/uapi/`：用户可见 ABI、flag 和结构定义；
- `arch/<arch>/`：架构 syscall/ABI 差异。

## 不应报告的情况

- Linux 内部实现不同，但外部行为等价；
- POSIX 允许多种行为，且 X-Kernel 选择其中一种；
- 仅凭旧版本 Linux 代码判断当前语义；
- 接口属于明确的 X-Kernel 私有扩展，并未声称 Linux 兼容；
- 没有给出输入、预期结果和实际结果的笼统“与 Linux 不一致”。

## 输出要求

遵循 `common.md` 的 finding JSON 格式。
评论应尽量包含接口名、触发输入、Linux/POSIX 预期、当前行为和用户可见后果。
引用规范时保持简洁，不要把整段 man page 粘贴进评论。
没有确认的兼容性问题时输出 `未发现问题`。

# fs_boot — 安全与可靠性分析

## 信任模型

root block device 名称来自内核构建配置；设备枚举和磁盘内容来自驱动及外部介质。
具体磁盘格式验证由 registry 中候选 `FileSystemType` 的 `get_tree/fill_super` 负责。
内建 filesystem type 描述符和 boot mount policy 是受信任的内核构建产物。

## 外部边界 / 攻击面

- `KFEAT_ROOT_BLOCK` 可能不匹配已注册设备。
- block device 可能缺失、被移除、返回 I/O 错误或包含损坏的文件系统。
- 9P mount tag 和响应来自外部 transport。

本 crate 不处理用户指针；调用发生在用户进程启动前。

## unsafe 代码清单

`fs/boot/src` 没有 `unsafe` block。

## 内存安全不变量

- bootstrap mount namespace 初始化成功后才能取得 initial root path。
- root device handle 在 `FsContext::get_tree` 和真实 root graft 完成前保持有效。
- init `FsStruct` 的 root/pwd 在 bootstrap 和真实 root 两次切换中都成对更新。
- 所有内建 filesystem type 必须在安装用户态可见的 procfs 和开放 mount syscall 前
  通过统一 init 段完成唯一注册。
- registry 必须保存各实现的唯一静态 descriptor 引用；boot 直接使用这些 canonical 对象，
  POSIX 和 procfs 通过 registry 查找/遍历同一对象，不复制类型或另建 provider。
- `nosuid`、`nodev`、`noexec`、`relatime` 属于具体 `Mount`；不得写入共享
  superblock 状态。`/dev` 不得设置 `nodev`。
- root 候选必须先按 RW 完整探测，再以 `SB_RDONLY` 和只读 mount flag 完整重试；不得因
  read-only block device 的首轮 `EACCES` 直接停止启动。
- 只读真实 root 必须预先包含 boot policy 所需的 mountpoint 目录；Linux 同样不会在
  只读介质上合成缺失的目录。目录缺失属于无效 root 镜像并停止启动。
- mount helper 只接受已存在目录；boot policy 声明创建的嵌套路径必须先逐级建立，不能只
  创建最终分量或删除同名非目录对象。
- boot 挂载的 devtmpfs/sysfs 必须与对应 type factory 使用同一共享 superblock，
  否则后续 mount 会暴露缺失 boot 节点的分离目录树。
- devtmpfs/sysfs 构造器必须保留 internal root mount 的 active 引用；可见 mount 卸载不得
  teardown 内核仍在更新的共享树。

## 线程安全

初始 mount 建立由 boot CPU 串行执行。已挂载文件系统直接持有 backing device handle；
boot 不另建移除订阅或设备生命周期状态。

## 威胁分析

| 编号 | 威胁描述 | 影响等级 | 触发条件 | 应对措施 |
|------|----------|----------|----------|----------|
| T-01 | root 类型选择与可挂载类型表漂移 | 高 | Kconfig、runtime 或 boot 维护第二套 provider/name/能力来源 | Kconfig 只链接实现；每个实现通过统一 init 段注册自己的 canonical descriptor；boot 只遍历同一 registry 的 `REQUIRES_DEV` 类型 |
| T-02 | 损坏磁盘触发不安全解析 | 高 | filesystem 未校验外部介质 | mount 错误必须传播；磁盘校验由具体 `fill_super` 实现 |
| T-03 | 已挂载 backing device 被移除 | 中 | 热移除 root 或 9P 设备 | mount/session 持有 resident object；后续 I/O 传播设备错误；boot 不复制设备生命周期状态 |
| T-04 | filesystem type 列表与用户 mount 分派漂移 | 中 | boot、procfs 和 syscall 各自维护类型名或构造入口 | boot 直接使用已注册 canonical descriptor，procfs/POSIX 从同一 registry 取得它，并统一经 `MntNamespace::mount_new` 创建和 attach |
| T-05 | 启动 mount flags 错误作用到共享 superblock 或禁用 `/dev` | 高 | 把 per-mount flags 填入 statfs 后端状态，或给 `/dev` 设置 `nodev` | boot 通过 namespace attach 设置 `MountFlags`；superblock 构造器只接收 filesystem-wide flags |
| T-06 | 只读 root device 无法启动 | 高 | root 探测只使用 RW flags，首轮 `EACCES` 后直接 panic | 按 Linux `mount_root_generic()` 对完整 filesystem 候选集执行 RW、RO 两轮探测；RO 轮同时设置 superblock 与 mount 只读位；root 镜像须预置必要 mountpoint |
| T-07 | 9P 嵌套挂载点缺少父目录 | 中 | mount helper 只对最终 `/mnt/hostshare` 执行 `mkdir` | `mount_host_share()` 在挂载前通过 `ensure_directory_path()` 逐级创建 policy-owned 路径；通用 helper 要求目标已存在 |

## 故障模式与影响分析（FMEA）

| 编号 | 故障模式 | 故障原因 | 局部影响 | 系统影响 | 严重度 | 应对措施 |
|------|----------|----------|----------|----------|--------|----------|
| F-01 | 无 block device | 驱动未探测或配置错误 | 无法选择 root backing | boot panic | 1 | 明确 `No block device found` |
| F-02 | root mount 失败 | source lookup、I/O、格式或 feature 错误 | 无真实 root superblock | boot panic | 1 | 记录 `FsContext/get_tree` 错误后停止启动 |
| F-03 | 首选设备名不存在 | 配置与硬件不一致 | 无法选择 root | boot panic | 2 | 输出配置名和候选设备，不回退到其他介质 |
| F-04 | filesystem type 重名或重复注册 | 启动接线错误 | 注册返回 `ResourceBusy` | boot 在 namespace 对用户态开放前停止 | 2 | 每个配置启用的 canonical type 恰好注册一次 |

## 故障管理

root/关键虚拟文件系统失败会停止启动，避免在不完整 namespace 上运行用户态。
可选 9P mount 错误保留 transport 上下文并停止启动。

## 已知限制

设备移除尚不能强制 mounted block filesystem 进入只读或完成自动卸载；已持有对象的
后续 I/O 会返回底层错误。未配置 root name 时仍保留按 `dev_t` 排序的兼容选择。filesystem type
当前是静态内建集合，不支持运行时注册/卸载模块。

## 审计清单

- `fs_boot` 是否仍然不引用具体 block filesystem crate 或 `KFEAT_FS_*` 分支？
- 新 mount path 是 namespace policy，还是应进入通用 mount/type 层？
- 新 filesystem 是否注册 canonical type，且没有在 procfs 或 POSIX 层增加名称表？
- 新启动 mount 的 flags 是否写入 `Mount`；`/dev` 是否保持可访问设备节点？
- 真实 root mount 失败是否在启动用户态前终止，并保留明确错误？
- backing device 生命周期是否由 mount/session handle 持有，而不是在 boot 复制状态？
